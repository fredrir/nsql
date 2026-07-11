use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Same pipeline as the normal-mode run (`rpcnotify nsql_run` → Rust-side guard →
/// results pane); only the SQL text differs.
pub(super) const VISUAL_RUN_LUA: &str = r#"
pcall(function()
  local ebuf = vim.g.nsql_ebuf or vim.api.nvim_get_current_buf()
  vim.keymap.set('x', ',,', function()
    local sql
    if vim.fn.exists('*getregion') == 1 then
      local ok, region = pcall(vim.fn.getregion, vim.fn.getpos('v'), vim.fn.getpos('.'),
        { type = vim.fn.mode() })
      if ok then sql = table.concat(region, '\n') end
    end
    if sql then
      local esc = vim.api.nvim_replace_termcodes('<Esc>', true, false, true)
      vim.api.nvim_feedkeys(esc, 'n', false)
    else
      vim.cmd('normal! "vy')
      sql = vim.fn.getreg('v')
    end
    local ch = vim.g.nsql_chan
    if ch then
      pcall(vim.rpcnotify, ch, 'nsql_run', { sql = sql or '', force = false, all = false })
    end
  end, { buffer = ebuf, silent = true, desc = 'nsql' })
end)
"#;

pub(super) fn translate_key(k: KeyEvent) -> Option<String> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let wrap = |s: String| -> String {
        let mut inner = s;
        if alt {
            inner = format!("M-{inner}");
        }
        if inner.len() > 1 || alt || ctrl {
            format!("<{inner}>")
        } else {
            inner
        }
    };
    let named = |name: &str| -> Option<String> {
        let mut inner = String::new();
        if ctrl {
            inner.push_str("C-");
        }
        if alt {
            inner.push_str("M-");
        }
        inner.push_str(name);
        Some(format!("<{inner}>"))
    };

    match k.code {
        KeyCode::Char(c) => {
            if ctrl {
                return Some(format!("<C-{c}>"));
            }
            if c == '<' {
                return Some("<lt>".to_string());
            }
            Some(wrap(c.to_string()))
        }
        KeyCode::Enter => named("CR"),
        KeyCode::Esc => named("Esc"),
        KeyCode::Backspace => named("BS"),
        KeyCode::Tab => named("Tab"),
        KeyCode::BackTab => Some("<S-Tab>".to_string()),
        KeyCode::Delete => named("Del"),
        KeyCode::Left => named("Left"),
        KeyCode::Right => named("Right"),
        KeyCode::Up => named("Up"),
        KeyCode::Down => named("Down"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PageUp"),
        KeyCode::PageDown => named("PageDown"),
        KeyCode::Insert => named("Insert"),
        KeyCode::F(n) => Some(format!("<F{n}>")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::session::{RedrawHandler, RunMsg};
    use nvim_rs::Value;
    use tokio::sync::mpsc;

    #[test]
    fn visual_double_comma_runs_only_the_selection() {
        if crate::util::find_on_path("nvim").is_none() {
            eprintln!("skip: nvim not on PATH");
            return;
        }
        use std::time::Duration;
        let sqlf = crate::util::secure_tempfile("nsql-vis", "sql").unwrap();
        std::fs::write(&sqlf, "select 1 as a;\nselect 2 as b;\n").unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let got = rt.block_on(async {
            let mut cmd = tokio::process::Command::new("nvim");
            cmd.arg("--embed").arg("--clean").arg(&sqlf);
            let (redraw_tx, _redraw_rx) = mpsc::unbounded_channel::<Vec<Value>>();
            let (run_tx, mut run_rx) = mpsc::unbounded_channel::<RunMsg>();
            let (nvim, _io, mut child) = nvim_rs::create::tokio::new_child_cmd(
                &mut cmd,
                RedrawHandler {
                    tx: redraw_tx,
                    run_tx,
                },
            )
            .await
            .expect("spawn nvim --embed");

            let mut o = nvim_rs::UiAttachOptions::new();
            o.set_linegrid_external(true);
            o.set_rgb(true);
            nvim.ui_attach(80, 6, &o).await.expect("attach");

            if let Ok(info) = nvim.get_api_info().await {
                if let Some(ch) = info.first().and_then(|v| v.as_i64()) {
                    let _ = nvim.set_var("nsql_chan", Value::from(ch)).await;
                }
            }
            nvim.exec_lua(VISUAL_RUN_LUA, vec![]).await.expect("lua");

            nvim.input("ggV,,").await.expect("input");
            let msg = tokio::time::timeout(Duration::from_secs(3), run_rx.recv())
                .await
                .ok()
                .flatten();

            nvim.command("qa!").await.ok();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            msg
        });
        std::fs::remove_file(&sqlf).ok();

        match got {
            Some(RunMsg::Run { sql, force, all }) => {
                assert!(sql.contains("select 1 as a;"), "unexpected sql: {sql:?}");
                assert!(
                    !sql.contains("select 2"),
                    "selection must exclude the unselected statement: {sql:?}"
                );
                assert!(!force && !all, "visual run must use the plain run variant");
            }
            _ => panic!("visual `,,` did not deliver a RunMsg::Run"),
        }
    }

    #[test]
    fn key_translation() {
        let plain = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(translate_key(plain).as_deref(), Some("a"));
        let cr = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(translate_key(cr).as_deref(), Some("<CR>"));
        let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(translate_key(ctrl_w).as_deref(), Some("<C-w>"));
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(translate_key(esc).as_deref(), Some("<Esc>"));
        let lt = KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE);
        assert_eq!(translate_key(lt).as_deref(), Some("<lt>"));
    }
}
