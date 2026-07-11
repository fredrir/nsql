use crate::config::Profile;
use crate::db;
use nvim_rs::Value;

/// Strictly on-demand (`<C-x><C-o>`) — no CursorMovedI/TextChangedI feeding; an
/// auto-popup variant doubled typed characters (see .docs/ERGONOMICS.md).
pub(super) const OMNI_LUA: &str = r#"
pcall(function()
  local ebuf = vim.g.nsql_ebuf or vim.api.nvim_get_current_buf()
  function _G.nsql_omni(findstart, base)
    if findstart == 1 then
      local line = vim.api.nvim_get_current_line()
      local col = vim.api.nvim_win_get_cursor(0)[2]
      local s = col
      while s > 0 and line:sub(s, s):match('[%w_]') do
        s = s - 1
      end
      return s
    end
    local ok, out = pcall(function()
      local schema = _G.nsql_schema
      if not schema then return {} end
      local b = (base or ''):lower()
      local items, seen = {}, {}
      local function add(list, kind)
        for _, w in ipairs(list or {}) do
          if not seen[w] and (b == '' or w:lower():sub(1, #b) == b) then
            seen[w] = true
            items[#items + 1] = { word = w, kind = kind }
          end
        end
      end
      add(schema.tables, 'T')
      add(schema.columns, 'c')
      return items
    end)
    if ok then return out end
    return {}
  end
  vim.bo[ebuf].omnifunc = 'v:lua.nsql_omni'
end)
"#;

pub(super) const SET_SCHEMA_LUA: &str = r#"
local s = ...
_G.nsql_schema = s
pcall(function()
  local ew = vim.g.nsql_ewin
  local buf = vim.g.nsql_ebuf
  if not (ew and vim.api.nvim_win_is_valid(ew)) then return end
  -- Overridable defaults; link to theme groups so tables vs columns vs keywords
  -- are all distinguishable.
  vim.api.nvim_set_hl(0, 'NsqlSchemaTable', { link = 'Type', default = true })
  vim.api.nvim_set_hl(0, 'NsqlSchemaColumn', { link = 'Identifier', default = true })

  local tset, cset = {}, {}
  for _, t in ipairs(s.tables or {}) do tset[t:lower()] = true end
  for _, c in ipairs(s.columns or {}) do cset[c:lower()] = true end
  local ns = vim.api.nvim_create_namespace('nsql_schema_hl')

  -- TREESITTER: walk the parse tree, colour leaf identifier nodes that match the
  -- schema, skipping string / comment / literal subtrees. Returns true if it ran.
  local skip = { string = true, comment = true, literal = true, string_literal = true,
                 marginalia = true, dollar_quote = true, ['string_content'] = true }
  local function ts_paint()
    if not buf or not vim.api.nvim_buf_is_valid(buf) then return false end
    local ok, parser = pcall(vim.treesitter.get_parser, buf, 'sql')
    if not ok or not parser then return false end
    local okp, trees = pcall(function() return parser:parse() end)
    if not okp or not trees[1] then return false end
    vim.api.nvim_buf_clear_namespace(buf, ns, 0, -1)
    local function walk(node, in_skip)
      local sk = in_skip or skip[node:type()] or false
      local has_named = false
      for child in node:iter_children() do
        if child:named() then has_named = true; walk(child, sk) end
      end
      if not has_named and not sk then
        local txt = vim.treesitter.get_node_text(node, buf)
        if txt and txt:match('^[%w_]+$') then
          local low = txt:lower()
          local hl = tset[low] and 'NsqlSchemaTable' or (cset[low] and 'NsqlSchemaColumn' or nil)
          if hl then
            local sr, sc, er, ec = node:range()
            pcall(vim.api.nvim_buf_set_extmark, buf, ns, sr, sc,
              { end_row = er, end_col = ec, hl_group = hl, priority = 150 })
          end
        end
      end
    end
    walk(trees[1]:root(), false)
    return true
  end

  -- FALLBACK: matchadd (whole-word, case-insensitive; colours everywhere including
  -- strings/comments — acceptable for a scratch buffer with no sql parser).
  local function matchadd_paint()
    local function pat(list, cap)
      local parts = {}
      for i = 1, math.min(#list, cap) do parts[#parts + 1] = (list[i]:gsub('\\', '\\\\')) end
      if #parts == 0 then return nil end
      return '\\c\\V\\<\\%(' .. table.concat(parts, '\\|') .. '\\)\\>'
    end
    local tp, cp = pat(s.tables or {}, 1000), pat(s.columns or {}, 2000)
    vim.api.nvim_win_call(ew, function()
      for _, m in ipairs(vim.fn.getmatches()) do
        if m.group == 'NsqlSchemaTable' or m.group == 'NsqlSchemaColumn' then
          pcall(vim.fn.matchdelete, m.id)
        end
      end
      if cp then pcall(vim.fn.matchadd, 'NsqlSchemaColumn', cp, 10) end
      if tp then pcall(vim.fn.matchadd, 'NsqlSchemaTable', tp, 11) end
    end)
  end

  if ts_paint() then
    -- keep it fresh on edits, debounced (coalesce to one repaint per 150ms).
    local pending = false
    vim.api.nvim_create_autocmd({ 'TextChanged', 'TextChangedI' }, {
      buffer = buf,
      callback = function()
        if pending then return end
        pending = true
        vim.defer_fn(function() pending = false; pcall(ts_paint) end, 150)
      end,
    })
  else
    matchadd_paint()
  end
end)
"#;

pub(super) fn introspect_schema(profile: &Profile) -> Option<Value> {
    let q = crate::introspect::completion_query(profile.scheme())?;
    let rows = match db::run(profile, q, true).ok()? {
        db::QueryResult::Rows { rows, .. } => rows,
        _ => return None,
    };
    let mut tables: Vec<String> = Vec::new();
    let mut by_table: Vec<(String, Vec<String>)> = Vec::new();
    let mut all_cols: Vec<String> = Vec::new();
    let mut seen_col = std::collections::HashSet::new();
    for row in &rows {
        let t = cell_str(row.first());
        let c = cell_str(row.get(1));
        if t.is_empty() {
            continue;
        }
        if by_table.last().map(|(n, _)| n != &t).unwrap_or(true) {
            tables.push(t.clone());
            by_table.push((t.clone(), Vec::new()));
        }
        if !c.is_empty() {
            by_table.last_mut().unwrap().1.push(c.clone());
            if seen_col.insert(c.clone()) {
                all_cols.push(c);
            }
        }
    }
    if tables.is_empty() {
        return None;
    }
    let arr = |v: &[String]| Value::Array(v.iter().map(|s| Value::from(s.as_str())).collect());
    let by_table_v = Value::Map(
        by_table
            .iter()
            .map(|(t, cols)| (Value::from(t.as_str()), arr(cols)))
            .collect(),
    );
    Some(Value::Map(vec![
        (Value::from("tables"), arr(&tables)),
        (Value::from("columns"), arr(&all_cols)),
        (Value::from("by_table"), by_table_v),
    ]))
}

fn cell_str(c: Option<&db::Cell>) -> String {
    match c {
        Some(db::Cell::Text(s)) => s.clone(),
        Some(db::Cell::Int(i)) => i.to_string(),
        Some(db::Cell::Real(f)) => f.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Noop;

    #[async_trait::async_trait]
    impl nvim_rs::Handler for Noop {
        type Writer = crate::embed::NvimWriter;
    }

    #[test]
    fn omnifunc_is_on_demand_prefix_matched_and_nil_safe() {
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
            let (nvim, _io, mut child) = nvim_rs::create::tokio::new_child_cmd(&mut cmd, Noop)
                .await
                .expect("spawn nvim --embed");

            nvim.exec_lua(OMNI_LUA, vec![]).await.expect("omni lua");

            let ofu = nvim
                .exec_lua("return vim.bo.omnifunc", vec![])
                .await
                .expect("omnifunc opt");
            assert_eq!(ofu.as_str(), Some("v:lua.nsql_omni"));

            let nil_case = nvim
                .exec_lua("return #_G.nsql_omni(0, 'ca')", vec![])
                .await
                .expect("nil schema call");
            assert_eq!(nil_case.as_i64(), Some(0), "nil schema must yield no items");

            nvim.exec_lua(
                "_G.nsql_schema = { tables = { 'cats', 'orders' }, columns = { 'name', 'CAmount' } }",
                vec![],
            )
            .await
            .expect("set schema");

            let items = nvim
                .exec_lua(
                    "local out = {} \
                     for _, it in ipairs(_G.nsql_omni(0, 'ca')) do out[#out+1] = it.word .. ':' .. it.kind end \
                     return table.concat(out, ',')",
                    vec![],
                )
                .await
                .expect("complete call");
            assert_eq!(items.as_str(), Some("cats:T,CAmount:c"));

            let autocmds = nvim
                .exec_lua(
                    "return #vim.api.nvim_get_autocmds({ event = { 'CursorMovedI', 'TextChangedI' } })",
                    vec![],
                )
                .await
                .expect("autocmd probe");
            assert_eq!(
                autocmds.as_i64(),
                Some(0),
                "omni completion must not wire any auto-popup triggers"
            );

            nvim.command("qa!").await.ok();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        });
    }

    #[test]
    fn introspect_schema_lists_tables_and_columns() {
        let path = std::env::temp_dir().join(format!("nsql-schema-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let prof = crate::config::Profile {
            name: "t".into(),
            url: format!("sqlite://{}", path.display()),
            prod: false,
            readonly: false,
            no_history: false,
            ssh: None,
        };
        db::run(&prof, "create table cat(name text, age int)", true).unwrap();
        db::run(&prof, "create table dog(id int, label text)", true).unwrap();

        let schema = introspect_schema(&prof).expect("schema");
        let get = |v: &Value, k: &str| -> Option<Value> {
            if let Value::Map(m) = v {
                m.iter()
                    .find(|(kk, _)| kk.as_str() == Some(k))
                    .map(|(_, vv)| vv.clone())
            } else {
                None
            }
        };
        let tables = get(&schema, "tables").unwrap();
        let tnames: Vec<&str> = tables
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.as_str())
            .collect();
        assert!(
            tnames.contains(&"cat") && tnames.contains(&"dog"),
            "tables: {tnames:?}"
        );

        let by_table = get(&schema, "by_table").unwrap();
        let cat_cols = get(&by_table, "cat").unwrap();
        let cnames: Vec<&str> = cat_cols
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c.as_str())
            .collect();
        assert!(
            cnames.contains(&"name") && cnames.contains(&"age"),
            "cat cols: {cnames:?}"
        );

        let _ = std::fs::remove_file(&path);
    }
}
