//! 网关 Web 状态页（根路径 `/`）：单文件内嵌 HTML，浏览器直访 `http://<host>:7864/`
//! 即可查看网关运行状态，替代此前根路径 404。
//!
//! 设计约束：
//! - 无前端构建依赖：HTML/CSS/JS 以常量内嵌本模块，`GET /` 直接返回，不引入
//!   静态文件服务与资产目录（桌面版数据目录结构保持不变）
//! - 数据源全部复用既有端点：`/health`（免鉴权，探活语义）+ `/status`、
//!   `/v1/models`（鉴权语义不变）；页面不新增任何后端路由与数据面
//! - 鉴权策略零变化：`/` 与 `/health` 同等免鉴权（仅展示探活级汇总，
//!   账号明细/模型目录仍需 API Key，浏览器无 Key 时对应区块提示输入）

/// 根路径状态页（单文件，无外部资源引用，CSP 友好）
pub const STATUS_PAGE_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>AI Work 助手 · API 网关状态</title>
<style>
:root { color-scheme: dark; --bg:#0f172a; --card:#1e293b; --line:#334155; --fg:#e2e8f0; --mut:#94a3b8; --ok:#34d399; --warn:#fbbf24; --bad:#f87171; --acc:#38bdf8; }
* { box-sizing:border-box; margin:0; }
body { background:var(--bg); color:var(--fg); font:14px/1.6 system-ui,"Segoe UI","Microsoft YaHei",sans-serif; padding:24px; max-width:1080px; margin:0 auto; }
h1 { font-size:20px; margin-bottom:4px; }
.sub { color:var(--mut); font-size:12px; margin-bottom:20px; }
.grid { display:grid; gap:12px; grid-template-columns:repeat(auto-fit,minmax(220px,1fr)); margin-bottom:16px; }
.card { background:var(--card); border:1px solid var(--line); border-radius:10px; padding:14px 16px; }
.kpi { font-size:26px; font-weight:600; }
.kpi.ok { color:var(--ok); } .kpi.warn { color:var(--warn); } .kpi.bad { color:var(--bad); }
.lbl { color:var(--mut); font-size:12px; }
.sec { background:var(--card); border:1px solid var(--line); border-radius:10px; padding:14px 16px; margin-bottom:16px; }
.sec h2 { font-size:14px; margin-bottom:10px; color:var(--acc); }
table { width:100%; border-collapse:collapse; font-size:13px; }
th,td { text-align:left; padding:6px 8px; border-bottom:1px solid var(--line); }
th { color:var(--mut); font-weight:500; }
.tag { display:inline-block; padding:1px 8px; border-radius:99px; font-size:12px; }
.tag.ok { background:rgba(52,211,153,.15); color:var(--ok); }
.tag.cooling { background:rgba(251,191,36,.15); color:var(--warn); }
.tag.bad { background:rgba(248,113,113,.15); color:var(--bad); }
.err { color:var(--bad); font-size:13px; }
.mut { color:var(--mut); }
#keybox { display:flex; gap:8px; margin-bottom:16px; align-items:center; }
#keybox input { flex:1; max-width:380px; background:var(--bg); border:1px solid var(--line); border-radius:8px; color:var(--fg); padding:7px 10px; font-size:13px; }
#keybox button { background:var(--acc); border:0; border-radius:8px; color:#082f49; padding:7px 14px; font-size:13px; cursor:pointer; }
.dot { display:inline-block; width:8px; height:8px; border-radius:99px; margin-right:6px; background:var(--bad); }
.dot.on { background:var(--ok); }
@media (max-width: 640px) {
  body { padding:14px; font-size:13px; }
  h1 { font-size:17px; }
  .sub { font-size:11px; }
  .kpi { font-size:20px; }
  .card, .sec { padding:10px 12px; }
  th,td { padding:4px 6px; font-size:12px; }
  #keybox { flex-wrap:wrap; }
  #keybox input { max-width:none; flex:1 1 100%; }
  #keybox button { flex:1 1 100%; }
}
</style>
</head>
<body>
<h1><span class="dot" id="dot"></span>API 网关状态</h1>
<div class="sub">AI Work 助手 · 数据每 5 秒自动刷新 · 账号明细与模型目录需 API Key（仅本机存储，不做任何外发）</div>

<div id="keybox">
  <input id="apikey" type="password" placeholder="可选：填入 API Key 查看账号明细 / 模型目录（Bearer）">
  <button onclick="loadAll()">加载</button>
</div>

<div class="grid" id="kpis"></div>

<div class="sec" id="sec-err" style="display:none"><h2>最近错误</h2><div class="err" id="lasterr"></div></div>

<div class="sec" id="sec-acc" style="display:none">
  <h2>Trae 池账号</h2><div style="overflow-x:auto"><table id="tbl-acc"></table></div>
  <h2 style="margin-top:14px">Buddy 池账号</h2><div style="overflow-x:auto"><table id="tbl-wb"></table></div>
</div>

<div class="sec" id="sec-models" style="display:none">
  <h2>模型目录</h2><div style="overflow-x:auto"><table id="tbl-models"></table></div>
</div>

<script>
const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? '—').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const key = () => sessionStorage.getItem('gw_key') || '';
const hdr = () => key() ? { 'Authorization': 'Bearer ' + key() } : {};
const kpi = (lbl, val, cls='') => `<div class="card"><div class="kpi ${cls}">${val}</div><div class="lbl">${lbl}</div></div>`;
const tag = (st) => ({available:'ok', ok:'ok'}[st] ? `<span class="tag ok">${esc(st)}</span>`
  : st==='cooling' ? '<span class="tag cooling">cooling</span>'
  : `<span class="tag bad">${esc(st)}</span>`);

async function j(url) {
  const r = await fetch(url, { headers: hdr() });
  if (r.status === 401) throw Object.assign(new Error('401'), { code: 401 });
  if (!r.ok) throw new Error('HTTP ' + r.status);
  return r.json();
}

function saveKey() { sessionStorage.setItem('gw_key', $('apikey').value.trim()); }

async function loadHealth() {
  try {
    const h = await j('/health');
    $('dot').classList.add('on');
    const p = h.pool || {}, w = h.wb || {};
    $('kpis').innerHTML =
      kpi('总请求', h.total_requests ?? 0) +
      kpi('Trae 池可用', `${p.available ?? 0} / ${p.total_accounts ?? 0}`, (p.available ?? 0) > 0 ? 'ok' : 'bad') +
      kpi('冷却 / 禁用', `${p.cooling ?? 0} / ${p.disabled ?? 0}`, (p.cooling ?? 0) + (p.disabled ?? 0) > 0 ? 'warn' : '') +
      kpi('Trae 池总积分', p.total_credits ?? '—') +
      (w.enabled ? kpi('Buddy 池可用', `${w.available ?? 0} / ${w.total_accounts ?? 0}`, (w.available ?? 0) > 0 ? 'ok' : 'bad') : '');
    $('sec-err').style.display = h.last_error ? '' : 'none';
    if (h.last_error) $('lasterr').textContent = h.last_error;
    return true;
  } catch (e) {
    $('kpis').innerHTML = kpi('网关状态', '不可达', 'bad');
    return false;
  }
}

async function loadDetail() {
  if (!key()) { $('sec-acc').style.display = 'none'; $('sec-models').style.display = 'none'; return; }
  try {
    const s = await j('/status');
    const row = (a) => `<tr><td>${esc(a.name)}</td><td>${tag(a.status || (a.disabled ? 'disabled' : a.cooling ? 'cooling' : 'available'))}</td><td>${a.credits ?? '—'}</td><td>${a.inflight ?? 0}</td><td>${a.err_count ?? 0}</td></tr>`;
    const tbl = (list) => `<tr><th>账号</th><th>状态</th><th>积分</th><th>并发</th><th>错误</th></tr>` + (list || []).map(row).join('');
    $('tbl-acc').innerHTML = tbl(s.accounts);
    $('tbl-wb').innerHTML = s.wb?.enabled ? tbl(s.wb.accounts) : '<tr><td class="mut">Buddy 池未启用</td></tr>';
    $('sec-acc').style.display = '';
    try {
      const m = await j('/v1/models');
      const mr = (x) => `<tr><td>${esc(x.id)}</td><td class="mut">${esc((x.sources || []).map(y => y.pool).join(', '))}</td></tr>`;
      $('tbl-models').innerHTML = '<tr><th>模型</th><th>来源池</th></tr>' + (m.data || []).map(mr).join('');
      $('sec-models').style.display = '';
    } catch (e) { $('sec-models').style.display = 'none'; }
  } catch (e) {
    if (e.code === 401) { $('sec-acc').style.display = 'none'; $('sec-models').style.display = 'none'; }
  }
}

function loadAll() { saveKey(); loadHealth(); loadDetail(); }
loadAll();
setInterval(loadAll, 5000);
</script>
</body>
</html>"#;
