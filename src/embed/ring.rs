use crate::config::Paths;
use crate::history;
use nvim_rs::Value;

const HIST_FETCH: usize = 50;

/// Pure read-projection of history.sqlite (bright line): ring capped at 9, no
/// tabline, no per-slot state beyond the buffer text; slot 0 is the live scratch.
pub(super) const SETUP_HISTORY_LUA: &str = r#"
pcall(function()
  local ebuf = vim.g.nsql_ebuf or vim.api.nvim_get_current_buf()
  local o = { buffer = ebuf, silent = true, desc = 'nsql' }
  local RING = 9
  local idx, live = 0, nil

  local function set_buf(lines)
    if vim.api.nvim_buf_is_valid(ebuf) then
      vim.api.nvim_buf_set_lines(ebuf, 0, -1, false, lines)
    end
  end
  local function to_lines(sql)
    return vim.split(sql, '\n', { plain = true })
  end

  local function cycle(delta)
    local es = _G.nsql_hist or {}
    local n = math.min(#es, RING)
    if n == 0 then return end
    if idx == 0 then
      live = vim.api.nvim_buf_get_lines(ebuf, 0, -1, false)
    end
    idx = (idx + delta) % (n + 1)
    if idx == 0 then
      set_buf(live or { '' })
    else
      set_buf(to_lines(es[idx]))
    end
  end
  vim.keymap.set('n', ',n', function() cycle(1) end, o)
  vim.keymap.set('n', ',p', function() cycle(-1) end, o)

  local function preview(sql)
    local one = sql:gsub('%s+', ' '):match('^%s*(.-)%s*$') or sql
    return vim.fn.strcharpart(one, 0, 80)
  end
  vim.keymap.set('n', '<C-r>', function()
    local es = _G.nsql_hist or {}
    if #es == 0 then
      vim.notify('nsql: no history yet', vim.log.levels.INFO)
      return
    end
    vim.ui.select(es, { prompt = 'nsql history', format_item = preview }, function(choice)
      if choice then
        idx, live = 0, nil
        set_buf(to_lines(choice))
      end
    end)
  end, o)
end)
"#;

pub(super) const SET_HISTORY_LUA: &str = r#"
local entries = ...
_G.nsql_hist = entries or {}
"#;

pub(super) fn entries_value(paths: &Paths, profile: &str) -> Value {
    let entries = history::recent_for(paths, profile, HIST_FETCH).unwrap_or_default();
    Value::Array(entries.into_iter().map(Value::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::session::RedrawHandler;
    use crate::embed::NvimWriter;
    use nvim_rs::Neovim;
    use tokio::sync::mpsc;

    async fn feed(nvim: &Neovim<NvimWriter>, keys: &str) {
        nvim.exec_lua(
            "local k = vim.api.nvim_replace_termcodes(..., true, false, true) \
             vim.api.nvim_feedkeys(k, 'mx', false)",
            vec![Value::from(keys)],
        )
        .await
        .expect("feed keys");
    }

    async fn buf_text(nvim: &Neovim<NvimWriter>) -> String {
        nvim.exec_lua(
            "return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), '\\n')",
            vec![],
        )
        .await
        .expect("read buffer")
        .as_str()
        .unwrap_or_default()
        .to_string()
    }

    #[test]
    fn ring_cycles_preserving_live_and_picker_replaces() {
        if crate::util::find_on_path("nvim").is_none() {
            eprintln!("skip: nvim not on PATH");
            return;
        }
        use std::time::Duration;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut cmd = tokio::process::Command::new("nvim");
            cmd.arg("--embed").arg("--clean");
            let (redraw_tx, _redraw_rx) = mpsc::unbounded_channel::<Vec<Value>>();
            let (run_tx, _run_rx) = mpsc::unbounded_channel();
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
            nvim.ui_attach(80, 6, &o).await.expect("attach");

            nvim.exec_lua(
                "vim.api.nvim_buf_set_lines(0, 0, -1, false, { '-- live' })",
                vec![],
            )
            .await
            .expect("seed buffer");
            nvim.exec_lua(SETUP_HISTORY_LUA, vec![])
                .await
                .expect("setup lua");

            feed(&nvim, ",n").await;
            assert_eq!(buf_text(&nvim).await, "-- live", "no history: ,n is a no-op");

            let entries = Value::Array(vec![
                Value::from("select 9 as newest"),
                Value::from("select   8\n  as old"),
            ]);
            nvim.exec_lua(SET_HISTORY_LUA, vec![entries])
                .await
                .expect("set entries");

            feed(&nvim, ",n").await;
            assert_eq!(buf_text(&nvim).await, "select 9 as newest");
            feed(&nvim, ",n").await;
            assert_eq!(buf_text(&nvim).await, "select   8\n  as old");
            feed(&nvim, ",n").await;
            assert_eq!(
                buf_text(&nvim).await,
                "-- live",
                "cycling around must restore the live scratch"
            );
            feed(&nvim, ",p").await;
            assert_eq!(
                buf_text(&nvim).await,
                "select   8\n  as old",
                ",p from live must wrap to the oldest slot"
            );
            feed(&nvim, ",p").await;
            feed(&nvim, ",p").await;
            assert_eq!(buf_text(&nvim).await, "-- live");

            nvim.exec_lua(
                "vim.ui.select = function(items, opts, cb) \
                   _G.nsql_test_fmt = opts.format_item(items[2]) \
                   cb(items[1]) \
                 end",
                vec![],
            )
            .await
            .expect("stub select");
            feed(&nvim, "<C-r>").await;
            assert_eq!(
                buf_text(&nvim).await,
                "select 9 as newest",
                "picking a history entry must replace the scratch content"
            );
            let fmt = nvim
                .exec_lua("return _G.nsql_test_fmt", vec![])
                .await
                .expect("fmt");
            assert_eq!(
                fmt.as_str(),
                Some("select 8 as old"),
                "previews must be one-line with whitespace collapsed"
            );

            nvim.command("qa!").await.ok();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        });
    }
}
