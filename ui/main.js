// Aria 播放器 UI。Tauri 全局 API(withGlobalTauri),无构建步骤。
'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const state = {
  path: null,
  duration: 0,
  playing: false,
  dragging: false,
  // 曲库(当前源)
  source: 'lazer',
  lib: null,           // sets[]
  libCollections: [],
  expanded: null,      // 展开难度的 set id
  nowTrack: null,      // { setId, sha2, qid } 正在播放的曲库曲目
  preview: null,       // { setId, sha2 } 右侧信息预览(单击行临时查看,不影响播放)
  // 播放列表(播放顺序;随机 = 洗好的顺序)
  queue: [],           // QueueItem[]
  muted: false,
  lastVol: 0.6,
  nowMods: 0,        // 当前播放条目的 mods 位(pb-mods 标签)
  qsel: null,        // 播放列表多选模式(null = 关;Set<qid>)
  quickAdd: true,    // 单击封面快速导入(不弹对话框)
  quickMods: 0,      // 快速导入应用的默认 mods 位
  dblAdd: false,       // 双击曲库行加入播放列表(默认关 = 双击播放)
};

const fmt = (ms) => {
  const s = Math.max(0, Math.round(ms / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
};
const fmtTotal = (ms) => {
  const min = Math.round(ms / 60000);
  if (min < 60) return `${min} 分钟`;
  return `${Math.floor(min / 60)} 小时 ${min % 60} 分`;
};
const basename = (p) => p.split(/[\\/]/).pop() || p;

// ---- mods(osu! legacy 位;与后端 wall.rs / 渲染端一致)----
const MOD_BITS = { HD: 8, HR: 16, EZ: 2, DT: 64, HT: 256, NC: 512 };
// 互斥组:难度类 HR↔EZ,速率类 DT/HT/NC 三选一
const MOD_EXCL = { HR: ['EZ'], EZ: ['HR'], DT: ['HT', 'NC'], HT: ['DT', 'NC'], NC: ['DT', 'HT'] };
function describeMods(bits) {
  return Object.keys(MOD_BITS).filter((k) => bits & MOD_BITS[k]).join(' ');
}
/// 播放条标题旁的当前 mods 标签
function applyNowMods(bits) {
  state.nowMods = bits || 0;
  const el = $('pb-mods');
  el.hidden = !state.nowMods;
  el.textContent = describeMods(state.nowMods);
}

// 播放速度预设(不变调变速:非 1× 时子进程用声码器补偿音调)
const SPEEDS = [0.5, 0.75, 1, 1.25, 1.5];
function applySpeedUi(v) {
  // 非预设值(如旧存档残留)就近吸附显示
  const best = SPEEDS.reduce((a, b) => (Math.abs(b - v) < Math.abs(a - v) ? b : a), 1);
  document.querySelectorAll('.seg-btn[data-speed]').forEach((b) => {
    b.classList.toggle('on', Number(b.dataset.speed) === best);
  });
  $('pb-speed').textContent = `${best}×`;
}

let toastTimer = 0;
function toast(msg) {
  const el = $('toast');
  el.textContent = msg;
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), 2600);
}

function setBadge(on, text) {
  $('badge').textContent = text ?? (on ? '运行中' : '未运行');
  $('badge').classList.toggle('off', !on);
}

const MODE_LABELS = { list_order: '顺序', random: '随机' };
const MODE_ORDER = ['list_order', 'random'];
function setModeLabel(mode) {
  $('pb-mode').textContent = `${MODE_LABELS[mode] ?? mode} ▸`;
}

// ---- Tab 切换 ----
function switchTab(name) {
  for (const btn of $('tabs').querySelectorAll('button')) {
    btn.classList.toggle('on', btn.dataset.tab === name);
  }
  for (const page of ['queue', 'library', 'settings', 'about']) {
    $(`tab-${page}`).classList.toggle('on', name === page);
  }
  if (name === 'library' && starRange.dirty) srRender();
}
$('tabs').querySelectorAll('button').forEach((btn) => {
  btn.addEventListener('click', () => switchTab(btn.dataset.tab));
});

// ---- 主题:暗/亮结构 × 主题色,自由组合 ----
// 变量定义在 style.css 顶部([data-mode]/[data-accent]);选择持久化在
// localStorage,index.html head 内的内联脚本在渲染前先行应用防闪烁。
const THEME_KEY = 'aria-theme';
const ACCENTS = {
  blue: { a: '#4da3ff', h: '#6db5ff', d: '#2f6fb8', c: '#06111f', label: '蓝' },
  pink: { a: '#ff66aa', h: '#ff7db5', d: '#c94e86', c: '#1a040d', label: '粉' },
  purple: { a: '#a06bff', h: '#b48aff', d: '#7451c2', c: '#14091f', label: '紫' },
  green: { a: '#3fd68f', h: '#62e0a6', d: '#2a9e68', c: '#04170e', label: '绿' },
  orange: { a: '#ff9a4d', h: '#ffae70', d: '#c86f2a', c: '#1f0e02', label: '橙' },
};
function applyTheme(mode, accent) {
  if (!ACCENTS[accent]) accent = 'blue';
  if (mode !== 'light' && mode !== 'dark') mode = 'dark';
  document.documentElement.dataset.mode = mode;
  document.documentElement.dataset.accent = accent;
  // 窗口框架(标题栏/边框)跟随主题:不设的话 Windows 按系统 App 模式来,
  // 亮色主题下边框会仍是系统的暗色
  try {
    window.__TAURI__.window.getCurrentWindow().setTheme(mode === 'light' ? 'light' : 'dark');
  } catch { /* 非窗口环境忽略 */ }
  try {
    localStorage.setItem(THEME_KEY, JSON.stringify({ mode, accent }));
  } catch { /* 忽略 */ }
  $('theme-mode').value = mode;
  for (const b of $('accent-row').querySelectorAll('.swatch')) {
    b.classList.toggle('on', b.dataset.accent === accent);
  }
}
{
  const row = $('accent-row');
  for (const [name, c] of Object.entries(ACCENTS)) {
    const b = document.createElement('button');
    b.className = 'swatch';
    b.dataset.accent = name;
    b.title = c.label;
    b.style.setProperty('--sw', c.a);
    b.addEventListener('click', () => {
      applyTheme(document.documentElement.dataset.mode || 'dark', name);
    });
    row.appendChild(b);
  }
  let saved = {};
  try { saved = JSON.parse(localStorage.getItem(THEME_KEY) || '{}'); } catch { /* 忽略 */ }
  applyTheme(saved.mode || 'dark', saved.accent || 'blue');
  $('theme-mode').addEventListener('change', () => {
    applyTheme($('theme-mode').value, document.documentElement.dataset.accent || 'blue');
  });
}

// 关于页外链:webview 内不能导航外域,统一交给系统默认浏览器
document.querySelectorAll('#tab-about a.about-item[href^="http"]').forEach((a) => {
  a.addEventListener('click', (ev) => {
    ev.preventDefault();
    invoke('open_url', { url: a.href }).catch((e) => toast(`${e}`));
  });
});

// ---- 播放器条 ----

function updatePlayUi() {
  $('toggle').textContent = state.playing ? '⏸' : '▶';
}

/// 曲目信息上屏(播放器条中央)
function setNowPlaying(title, sub) {
  $('pb-title-text').textContent = title;
  $('pb-title-text').parentElement.title = title;
  $('pb-sub').textContent = sub;
}

/// 从曲库数据找当前曲目信息(难度缺省时以最难档近似,wall://track 会带
/// 实际解析结果覆盖)
function describeTrack(setId, sha2) {
  const set = state.lib?.find((s) => s.id === setId);
  if (!set) return null;
  const title = set.titleUnicode || set.title;
  const artist = set.artistUnicode || set.artist;
  let b = sha2 ? set.beatmaps.find((x) => x.sha2 === sha2) : null;
  if (!b && set.beatmaps.length) {
    b = set.beatmaps.reduce((m, x) => (x.starRating > m.starRating ? x : m), set.beatmaps[0]);
  }
  return {
    title,
    artist,
    creator: set.creator,
    diff: b ? { name: b.name, star: b.starRating } : null,
  };
}

/// 播放条一行副标题(紧凑):作者 · 难度 · ★
function subOf(info) {
  if (!info) return '';
  const d = info.diff
    ? ` · ${info.diff.name}${info.diff.star > 0 ? ` · ★${info.diff.star.toFixed(2)}` : ''}`
    : '';
  return `${info.artist}${d}`;
}

/// 右侧封面下元信息:作者 / 难度 分行展示(谱师在信息卡),点击行复制
function renderCoverMeta(info) {
  const set = (id, text) => {
    const el = $(id);
    el.textContent = text || '';
    el.style.display = text ? '' : 'none';
  };
  set('meta-artist', info?.artist);
  set('meta-diff', info?.diff
    ? `${info.diff.name}${info.diff.star > 0 ? ` · ★${info.diff.star.toFixed(2)}` : ''}`
    : null);
}

/// 播放条难度选择器:列出当前谱面集全部难度;切换 = 锁定为该档并
/// 原位重播(等同右键「修改…」,条目参数同步更新)。
function updateDiffSelect(setId, sha2) {
  const sel = $('pb-diff');
  const set = setId ? state.lib?.find((s) => s.id === setId) : null;
  if (!set) {
    sel.innerHTML = '<option value="">难度</option>';
    sel.disabled = true;
    return;
  }
  const diffs = [...set.beatmaps].sort((a, b) => a.starRating - b.starRating);
  sel.innerHTML = '';
  for (const b of diffs) {
    const opt = document.createElement('option');
    opt.value = b.sha2;
    const star = b.starRating > 0 ? ` ★${b.starRating.toFixed(2)}` : '';
    opt.textContent = `${b.name}${star}`;
    sel.appendChild(opt);
  }
  // 没有「自动」选项:难度在导入/右键时锁定,条目恒有具体难度。
  // 当前 sha2 不在列表(历史谱面集级条目)→ 显示默认难度挑的那档
  // (即实际播放的难度)
  const shown = diffs.some((b) => b.sha2 === sha2)
    ? sha2
    : (pickDiffLocal(diffs, policyTarget())?.sha2 ?? '');
  sel.value = shown;
  sel.disabled = diffs.length === 0;
  sel.title = '切换难度 = 锁定为该档并重播(等同右键修改)';
}

$('pb-diff').addEventListener('change', async () => {
  const setId = state.nowTrack?.setId;
  if (!setId) return;
  const sha2 = $('pb-diff').value || null;
  state.nowTrack = { setId, sha2 };
  try {
    // 切难度 = 更新条目锁定难度并原位重播(等同右键「修改…」);
    // 临时播放(不在播放列表)退回点播
    const cur = state.queue?.find((t) => t.current);
    if (cur) {
      await invoke('queue_set_entry', { qid: cur.qid, mods: cur.mods, sha2 });
      toast('已切换难度(条目已更新)');
      refreshQueue();
    } else {
      await invoke('library_play', { setId, sha2 });
    }
  } catch (e) {
    toast(`切换难度失败：${e}`);
  }
});

function onEvent(ev) {
  switch (ev.event) {
    case 'ready':
      setBadge(true);
      $('fps').options[0].textContent = `跟随刷新率 (${ev.refreshHz}Hz)`;
      break;
    case 'loaded':
      state.duration = ev.durationMs;
      state.playing = true;
      setBadge(true);
      applyNowMods(ev.mods);
      refreshQueue();
      // 以后端为真源对齐当前曲目(队列自动切歌/恢复/手动选难度都会先
      // 落到 lazer_set/lazer_sha2 再载入):手动难度跟着显示,谱面集级
      // /策略挑选回到「自动」;手动文件播放 = None 走文件名兜底
      invoke('playing_track')
        .then((cur) => {
          state.nowTrack = cur?.setId ? { setId: cur.setId, sha2: cur.sha2 ?? null } : null;
          const info = describeTrack(state.nowTrack?.setId, state.nowTrack?.sha2);
          if (info) {
            setNowPlaying(info.title, subOf(info));
          } else {
            setNowPlaying(basename(ev.path), (ev.diff ? basename(ev.diff) + ' · ' : '') + (ev.hasAudio ? '有声' : '无声'));
            refreshNowPlaying();
          }
          setNowPlayingCover(state.nowTrack?.setId);
          updateDiffSelect(state.nowTrack?.setId, state.nowTrack?.sha2);
        })
        .catch(() => {});
      updatePlayUi();
      break;
    case 'status':
      state.duration = ev.durationMs;
      setBadge(true, ev.playing ? '运行中' : '已暂停');
      state.playing = ev.playing;
      // userSpeed = 用户倍速(DT/HT 的 rate 不掺入,按钮显示才不被吸附)
      if (typeof ev.userSpeed === 'number') applySpeedUi(ev.userSpeed);
      else if (typeof ev.speed === 'number') applySpeedUi(ev.speed);
      if (!state.dragging) {
        $('pb-seek').value = ev.durationMs > 0 ? Math.round((ev.tMs / ev.durationMs) * 1000) : 0;
      }
      $('pb-time-cur').textContent = fmt(ev.tMs);
      $('pb-time-total').textContent = fmt(ev.durationMs);
      updatePlayUi();
      break;
    case 'ended':
      break; // 播放结束静默处理,不弹提示
    case 'unloaded':
      setBadge(false, '空闲');
      setNowPlaying('未播放', '');
      applyNowMods(0);
      state.preview = null;
      $('pb-cover').hidden = true;
      $('pb-time-cur').textContent = '0:00';
      $('pb-time-total').textContent = '0:00';
      $('pb-seek').value = 0;
      $('info-card').hidden = true;
      updateDiffSelect(null);
      break;
    case 'detached':
      toast('桌面层已重启，正在恢复壁纸…');
      break;
    case 'error':
      toast(`错误：${ev.message}`);
      break;
    case 'exited':
      if ($('badge').textContent !== '已恢复桌面') setBadge(false);
      break;
  }
}

// ---- 曲库(当前源) ----

async function loadLibrary(force) {
  $('lib-list').innerHTML =
    '<div style="padding:24px;text-align:center;color:#8b8b9b">读取曲库中…（首次约数秒）</div>';
  $('lib-foot').textContent = '';
  try {
    const lib = await invoke('library_refresh', { refresh: force, source: state.source });
    state.lib = lib.sets;
    state.libCollections = lib.collections;
    const n = lib.sets.length;
    const srcName = lib.source === 'stable' ? 'osu!stable' : 'osu!lazer';
    $('lib-src').textContent = lib.realmPath
      ? `${srcName} · ${n} 个谱面集`
      : `${srcName} · 无谱面`;
    $('lib-src').title = lib.realmPath || '';
    // 源切换可用性
    for (const btn of $('src-switch').querySelectorAll('button')) {
      const s = btn.dataset.src;
      btn.classList.toggle('on', s === lib.source);
      const ok = s === 'stable' ? lib.stableAvailable : lib.lazerAvailable;
      btn.disabled = !ok;
      btn.title = ok
        ? (s === 'stable' ? 'osu!stable 曲库(osu!.db)' : 'osu!lazer 曲库(client.realm)')
        : '未检测到该源的数据目录';
    }
    // 收藏夹下拉
    const sel = $('lib-collection');
    const cur = sel.value;
    sel.innerHTML = '';
    for (const opt of [['', '全部谱面'], ...state.libCollections.map((c) => [c.name, `${c.name} (${c.md5s.length})`])]) {
      const o = document.createElement('option');
      o.value = opt[0];
      o.textContent = opt[1];
      sel.appendChild(o);
    }
    if ([...sel.options].some((o) => o.value === cur)) sel.value = cur;
    state.nowTrack = null;
    state.preview = null;
    renderLibList();
    refreshQueue();
  } catch (e) {
    $('lib-list').innerHTML =
      `<div style="padding:24px;text-align:center;color:#d98a7f">曲库不可用：${esc(String(e))}</div>`;
    $('lib-foot').textContent = '';
  }
}

async function switchSource(source) {
  if (source === state.source) return;
  try {
    await invoke('source_set', { source });
    state.source = source;
    state.nowTrack = null;
    toast(source === 'stable' ? '已切换到 osu!stable' : '已切换到 osu!lazer');
    await loadLibrary(false);
  } catch (e) {
    toast(`切换失败：${e}`);
  }
}

function starText(v) {
  return v > 0 ? `★${v.toFixed(2)}` : '★—';
}

// 曲库排序。默认按导入时间降序
// (最新导入在前):lazer = realm DateAdded,stable ≈ 谱面集目录 mtime。
// collection_order = 收藏夹内添加顺序(读取顺序):谱面集取其难度 md5
// 在当前收藏夹列表里最早出现的下标,升序 = 最早添加在前;未选收藏夹
// 时无意义,全部并列退回原序。字符串用 localeCompare(中文拼音友好),
// 数值直接比。star = 谱面集最高难度星级:降序 = 最难在前。
const libSort = { key: 'date_added', desc: true };
function sortSets(sets) {
  const key = libSort.key;
  let collOrder = null; // md5 → 收藏夹内添加顺序下标
  if (key === 'collection_order') {
    const name = $('lib-collection').value;
    const md5s = name ? state.libCollections?.find((c) => c.name === name)?.md5s : null;
    collOrder = new Map((md5s ?? []).map((m, i) => [m, i]));
  }
  const valueOf = (s) => {
    switch (key) {
      case 'title': return (s.titleUnicode || s.title).toLowerCase();
      case 'creator': return (s.creator || '').toLowerCase();
      case 'date_added': return s.dateAddedMs || 0;
      case 'star': return s.beatmaps.reduce((m, b) => Math.max(m, b.starRating || 0), 0);
      case 'size': return s.files.reduce((sum, f) => sum + (f.size || 0), 0);
      case 'collection_order': {
        if (!collOrder?.size) return Number.MAX_SAFE_INTEGER;
        let best = Number.MAX_SAFE_INTEGER;
        for (const b of s.beatmaps) {
          const i = collOrder.get(b.md5);
          if (i != null && i < best) best = i;
        }
        return best;
      }
      default: return (s.artistUnicode || s.artist).toLowerCase();
    }
  };
  const factor = libSort.desc ? -1 : 1;
  return sets
    .map((s, i) => ({ s, i }))
    .sort((a, b) => {
      const va = valueOf(a.s), vb = valueOf(b.s);
      let cmp;
      if (typeof va === 'string' || typeof vb === 'string') {
        cmp = String(va).localeCompare(String(vb), 'zh-Hans-CN', { numeric: true });
      } else {
        cmp = va < vb ? -1 : va > vb ? 1 : 0;
      }
      return cmp * factor || a.i - b.i;
    })
    .map((x) => x.s);
}

/// 当前过滤条件命中的谱面集(搜索 + 星级下限 + 收藏夹;不过滤返回全量)。
/// 渲染(带上限)与「添加到播放列表」批量入列共用。
function filteredSets() {
  const q = $('lib-search').value.trim().toLowerCase();
  const lo = starRange.lo, hi = starRange.hi;
  const collName = $('lib-collection').value;
  const md5s = collName
    ? new Set((state.libCollections.find((c) => c.name === collName)?.md5s) ?? [])
    : null;
  const filtered = state.lib.filter((set) => {
    const diffs = set.beatmaps;
    if (lo > 0 || hi !== null) {
      const inRange = diffs.some((b) => b.starRating >= lo && (hi === null || b.starRating <= hi));
      if (!inRange) return false;
    }
    if (md5s && !diffs.some((b) => md5s.has(b.md5))) return false;
    // 搜索只看标题/艺术家,Unicode 与英文同时匹配(任一命中即可)
    if (q) {
      const hay = `${set.titleUnicode} ${set.title} ${set.artistUnicode} ${set.artist}`.toLowerCase();
      if (!hay.includes(q)) return false;
    }
    return true;
  });
  return sortSets(filtered);
}

/// 收藏夹筛选当前命中的难度(按 md5);未启用收藏筛选 = null,
/// 调用方回退"全集按策略挑"的默认路径。
function collFilterDiffs(set) {
  const collName = $('lib-collection').value;
  if (!collName || !state.libCollections) return null;
  const md5s = new Set((state.libCollections.find((c) => c.name === collName)?.md5s) ?? []);
  if (!md5s.size) return null;
  return set.beatmaps.filter((b) => md5s.has(b.md5));
}

/// 按策略在给定难度里挑一档(与后端 pick_diff 同语义):
/// null=最难,-1=最简单,x>0=星级最接近。
function pickDiffLocal(diffs, target) {
  if (!diffs?.length) return null;
  let best = diffs[0];
  for (const b of diffs) {
    const better = target == null
      ? b.starRating > best.starRating
      : target < 0
        ? b.starRating < best.starRating
        : Math.abs(b.starRating - target) < Math.abs(best.starRating - target);
    if (better) best = b;
  }
  return best;
}

/// 收藏筛选场景下导入应锁定的难度:命中 1 个 = 直接该档;命中多个 =
/// 按设置的默认难度策略在命中难度内挑最相近;未启用筛选 = null
/// (走后端全集按策略挑选)。
function collPickSha2(set, target) {
  const cand = collFilterDiffs(set);
  if (!cand) return null;
  return (cand.length === 1 ? cand[0] : pickDiffLocal(cand, target)).sha2;
}

// 曲库行封面:快速导入(默认选项)或打开加入对话框(捕获阶段拦截,
// 不触发行的单击预览/双击播放)。「默认导入选项」开启时直接入列:
// 无 mods + 锁定默认难度值最相近的难度。
$('lib-list').addEventListener('click', (ev) => {
  const img = ev.target.closest('.lib-cover');
  if (!img || !img.dataset.cover) return;
  ev.stopPropagation();
  if (state.quickAdd) {
    const set = state.lib?.find((x) => x.id === img.dataset.cover);
    const sha2 = set ? collPickSha2(set, policyTarget()) : null;
    invoke('playlist_add', { setId: img.dataset.cover, sha2, mods: state.quickMods, target: sha2 ? null : policyTarget() })
      .then((added) => {
        toast(added ? '已添加到播放列表' : '已替换现有条目参数');
        refreshQueue();
      })
      .catch((e) => toast(`添加失败:${e}`));
    return;
  }
  openModsModal({ mode: 'add', setId: img.dataset.cover });
}, true);
$('lib-list').addEventListener('dblclick', (ev) => {
  if (ev.target.closest('.lib-cover')) ev.stopPropagation();
}, true);

// 单击/双击区分:单击延迟 260ms 生效,双击取消单击
const CLICK_DELAY = 260;
function onClickDblClick(el, onClick, onDblClick) {
  let timer = 0;
  el.addEventListener('click', () => {
    timer = setTimeout(onClick, CLICK_DELAY);
  });
  el.addEventListener('dblclick', () => {
    clearTimeout(timer);
    onDblClick();
  });
}

/// 曲库行:单击右侧预览(再点切回);双击播放(设置开启时同时加入播放列表)
function bindLibRow(el, setId, sha2) {
  onClickDblClick(el,
    () => previewSet(setId, sha2),
    () => {
      if (state.dblAdd) {
        invoke('playlist_add', { setId, sha2: sha2 ?? null, mods: 0 })
          .then(() => {
            toast('已加入播放列表');
            refreshQueue();
          })
          .catch((e) => toast(`加入失败:${e}`));
      }
      playSet(setId, sha2);
    });
}

// ---- 曲库列表渲染:滚动分批懒加载(无显示上限,首批覆盖正在播放/预览/展开行) ----
const LIB_BATCH = 100;
let libHits = []; // 当前过滤 + 排序后的全集
let libRendered = 0;
const libScrollObserver = new IntersectionObserver((entries) => {
  if (entries.some((en) => en.isIntersecting)) renderLibMore();
});

function buildLibRow(set) {
  const title = set.titleUnicode || set.title;
  const artist = set.artistUnicode || set.artist;
  const diffs = [...set.beatmaps].sort((a, b) => a.starRating - b.starRating);
  const row = document.createElement('div');
  row.className = 'lib-row'
    + (state.nowTrack?.setId === set.id ? ' playing' : '')
    + (state.preview?.setId === set.id ? ' previewing' : '');
  row.dataset.set = set.id;

  const head = document.createElement('div');
  head.className = 'lib-row-head';
  const isPlayingSet = state.nowTrack?.setId === set.id;
  const maxStar = diffs.length ? diffs[diffs.length - 1].starRating : 0;
  const starSpan = diffs.length
    ? `<b>${starText(diffs[0].starRating)}</b>–<b>${starText(maxStar)}</b>`
    : '';
  head.dataset.set = set.id;
  head.innerHTML =
    `<img class="lib-cover loading" alt="" data-cover="${set.id}" />` +
    `<div class="lib-meta">` +
    `<span class="lib-title">${isPlayingSet ? '▶ ' : ''}${esc(title)} <span class="dim">— ${esc(artist)}</span></span>` +
    `<span class="lib-artist">${diffs.length} 难度 · ${esc(set.creator)}</span>` +
    `</div>` +
    `<span class="lib-stars">${starSpan}</span>`;
  bindLibRow(head, set.id, null);
  const chevron = document.createElement('button');
  chevron.className = 'chevron';
  chevron.textContent = state.expanded === set.id ? '▾' : '▸';
  chevron.addEventListener('click', (ev) => {
    ev.stopPropagation();
    state.expanded = state.expanded === set.id ? null : set.id;
    renderLibList();
  });
  chevron.addEventListener('dblclick', (ev) => ev.stopPropagation());
  head.appendChild(chevron);
  row.appendChild(head);

  if (state.expanded === set.id) {
    const wrap = document.createElement('div');
    wrap.className = 'lib-diffs';
    for (const b of diffs) {
      const chip = document.createElement('button');
      chip.className = 'chip' + (state.nowTrack?.setId === set.id && state.nowTrack?.sha2 === b.sha2 ? ' on' : '');
      chip.dataset.set = set.id;
      chip.dataset.sha2 = b.sha2;
      chip.title = '右键:以此难度加入播放列表';
      chip.innerHTML = `<span class="star">${starText(b.starRating)}</span>${esc(b.name)}`;
      bindLibRow(chip, set.id, b.sha2);
      wrap.appendChild(chip);
    }
    row.appendChild(wrap);
  }
  return row;
}

function appendLibRows(list, from, to) {
  const frag = document.createDocumentFragment();
  for (let i = from; i < to; i++) frag.appendChild(buildLibRow(libHits[i]));
  const sentinel = list.querySelector('.lib-more');
  list.insertBefore(frag, sentinel ?? null);
  list.querySelectorAll('img[data-cover]').forEach((img) => coverObserver.observe(img));
}

function updateLibSentinel(list) {
  list.querySelector('.lib-more')?.remove();
  if (libRendered < libHits.length) {
    const more = document.createElement('div');
    more.className = 'queue-more dim';
    more.textContent = `↓ 滚动加载更多(已显示 ${libRendered} / ${libHits.length})`;
    list.appendChild(more);
    libScrollObserver.observe(more);
    $('lib-foot').textContent = `已显示 ${libRendered} / ${libHits.length}`;
  } else {
    const filtered = libHits.length !== state.lib.length
      ? `（过滤后 ${libHits.length}）`
      : '';
    $('lib-foot').textContent = libHits.length
      ? `共 ${state.lib.length} 个谱面集${filtered}`
      : (state.lib.length ? '过滤条件下没有谱面集' : '');
  }
}

function renderLibMore() {
  const list = $('lib-list');
  if (libRendered >= libHits.length) return;
  const to = Math.min(libHits.length, libRendered + LIB_BATCH);
  appendLibRows(list, libRendered, to);
  libRendered = to;
  updateLibSentinel(list);
}

function renderLibList() {
  const list = $('lib-list');
  libScrollObserver.disconnect();
  list.innerHTML = '';
  if (!state.lib) {
    libHits = [];
    libRendered = 0;
    return;
  }
  libHits = filteredSets();
  // 首批:开头 → 正在播放/预览/展开行(再带余量),更深的行滚动续载
  const focusIdx = Math.max(
    libHits.findIndex((s) => s.id === state.nowTrack?.setId),
    libHits.findIndex((s) => s.id === state.preview?.setId),
    libHits.findIndex((s) => s.id === state.expanded),
  );
  const initial = Math.max(LIB_BATCH, (focusIdx >= 0 ? focusIdx : 0) + LIB_BATCH / 2);
  const to = Math.min(libHits.length, initial);
  appendLibRows(list, 0, to);
  libRendered = to;
  updateLibSentinel(list);
}

// 「添加到播放列表」:先弹 mods 对话框(批量统一 mods;加入单位恒为
// 谱面集级),确认后把当前过滤命中的全部谱面集批量入列
$('lib-add').addEventListener('click', () => {
  const hits = filteredSets();
  if (!hits.length) {
    toast('当前过滤条件下没有谱面');
    return;
  }
  openModsModal({ mode: 'batch', count: hits.length });
});

// ---- 播放列表(主 tab) ----

async function refreshQueue() {
  try {
    state.queue = await invoke('queue');
    renderQueue();
    if (state.nowTrack?.setId) {
      updateDiffSelect(state.nowTrack.setId, state.nowTrack.sha2);
    }
  } catch {
    /* 曲库未就绪等,静默 */
  }
}

// ---- 播放列表渲染:完整列表 + 懒加载 ----
// 首批渲染开头到当前曲目(播放中的歌始终可见),滚动触底自动追加下一批;
// 封面仍由 IntersectionObserver 按可见性加载,几千行的列表也不卡。
const QUEUE_BATCH = 200;
let queueRendered = 0; // 已渲染行数

const queueScrollObserver = new IntersectionObserver((entries) => {
  if (entries.some((en) => en.isIntersecting)) renderQueueMore();
}, { root: $('queue-list'), rootMargin: '600px' });

function appendQueueRows(list, from, to) {
  const items = state.queue;
  const frag = document.createDocumentFragment();
  for (let idx = from; idx < to; idx++) {
    const t = items[idx];
    const row = document.createElement('div');
    const sel = !!state.qsel?.has(t.qid);
    row.className = 'queue-row'
      + (t.current ? ' on' : '')
      + (state.preview?.setId === t.setId ? ' previewing' : '')
      + (sel ? ' selected' : '');
    row.dataset.set = t.setId;
    row.dataset.qid = t.qid;
    row.dataset.index = idx;
    const star = t.star > 0 ? ` <span class="q-star">★${t.star.toFixed(2)}</span>` : '';
    const dur = t.lengthMs > 0 ? fmt(t.lengthMs) : '';
    // 快照模式(曲库未就绪)标题可能为空(旧存档无快照):显示占位
    const title = t.title || '…';
    const sub = t.artist || '曲库加载中';
    const modsTag = t.mods ? `<span class="mod-tag">${describeMods(t.mods)}</span>` : '';
    row.innerHTML =
      `<span class="q-check${sel ? ' on' : ''}"></span>` +
      `<img class="q-cover loading" alt="" data-cover="${t.setId}" />` +
      `<span class="q-idx">${t.current ? '▶' : idx + 1}</span>` +
      `<div class="q-meta"><span class="q-title">${esc(title)}${star}${modsTag}</span>` +
      `<span class="q-sub">${esc(sub)}${t.diff ? ' · ' + esc(t.diff) : ''}</span></div>` +
      `<span class="q-dur dim">${dur}</span>` +
      `<button class="q-del" title="移除">✕</button>`;
    // 拖动排序手柄(顺序模式;随机模式隐藏)
    if (state.libMode !== 'random') {
      const handle = document.createElement('span');
      handle.className = 'q-drag';
      handle.textContent = '⠿';
      handle.title = '拖动排序';
      handle.addEventListener('pointerdown', (ev) => startQueueDrag(ev, handle, row));
      // 不触发行的预览/播放
      handle.addEventListener('click', (ev) => ev.stopPropagation());
      handle.addEventListener('dblclick', (ev) => ev.stopPropagation());
      row.prepend(handle);
    }
    // 多选模式:点击 = 切换选中(预览/播放/删除/拖动均让位);
    // 常态:单击右侧预览,双击播放
    if (state.qsel) {
      row.addEventListener('click', () => toggleQSel(t.qid));
    } else {
      onClickDblClick(row, () => previewSet(t.setId, t.sha2), () => playSet(t.setId, t.sha2));
    }
    row.querySelector('.q-del').addEventListener('click', async (ev) => {
      ev.stopPropagation();
      try {
        await invoke('playlist_remove', { qid: t.qid });
        refreshQueue();
      } catch (e) {
        toast(`${e}`);
      }
    });
    frag.appendChild(row);
  }
  list.appendChild(frag);
  list.querySelectorAll('img[data-cover]').forEach((img) => coverObserver.observe(img));
}

// ---- 播放列表拖动排序(顺序模式):手柄起拖,行跟随 + 邻行让位动画 ----
// 几何量只在起拖时测量一次;拖动期间 transform 不影响布局,全部纯算术
// 定位(每次 pointermove 零 DOM 查询/零重排),跟手不掉帧。
function startQueueDrag(ev, handle, row) {
  if (state.libMode !== 'list_order') return;
  ev.preventDefault();
  const list = $('queue-list');
  const sentinel = list.querySelector('.queue-more');
  handle.setPointerCapture(ev.pointerId);

  const listTop = list.getBoundingClientRect().top;
  // 槽位纵向位置一律用**容器内容坐标**(随滚动平移的量都已换算),
  // 拖动期间 transform 不影响布局,可全程纯算术定位。
  const rows = [...list.querySelectorAll('.queue-row')];
  const n = rows.length;
  const T = rows.map((r) => r.getBoundingClientRect().top - listTop + list.scrollTop);
  const h = row.getBoundingClientRect().height;
  const mid = (i) => T[i] + h / 2; // 槽位中线(固定值)
  const startY = ev.clientY - listTop + list.scrollTop; // 起点内容坐标
  const from = rows.indexOf(row);
  const homeMid = T[from] + h / 2;

  let p = from;             // 拖动行当前占据的槽位
  const seq = rows.slice(); // 槽位 → 行元素(视觉顺序)
  let started = false;
  let lastY = ev.clientY;

  const update = (clientY) => {
    const dy = (clientY - listTop + list.scrollTop) - startY;
    const center = homeMid + dy;
    row.style.transform = `translateY(${dy}px)`;
    // 跨过相邻槽位中线才让位(逐槽交换,无 DOM 移动)
    while (p < n - 1 && center > mid(p + 1)) {
      const other = seq[p + 1];
      seq[p + 1] = seq[p]; seq[p] = other;
      other.style.transform = `translateY(${T[p] - T[p + 1]}px)`;
      p++;
    }
    while (p > 0 && center < mid(p)) {
      const other = seq[p - 1];
      seq[p - 1] = seq[p]; seq[p] = other;
      other.style.transform = `translateY(${T[p] - T[p - 1]}px)`;
      p--;
    }
  };
  const onScroll = () => update(lastY); // 拖动中滚轮/滚动条:行钉在光标下
  const onMove = (e) => {
    if (!started) {
      if (Math.abs(e.clientY - startY) < 4) return; // 点按不误拖
      started = true;
      row.classList.add('dragging');
      row.style.transition = 'none';
      for (const r of rows) if (r !== row) r.style.transition = 'transform .16s ease';
    }
    lastY = e.clientY;
    update(e.clientY);
  };
  const finish = () => {
    handle.removeEventListener('pointermove', onMove);
    handle.removeEventListener('pointerup', finish);
    handle.removeEventListener('pointercancel', finish);
    list.removeEventListener('scroll', onScroll);
    row.classList.remove('dragging');    if (!started) return;
    // 按最终视觉顺序把 DOM 归位(视觉不变),同时清掉全部临时变换
    for (let i = 0; i < n; i++) {
      seq[i].style.transform = '';
      seq[i].style.transition = '';
      list.insertBefore(seq[i], sentinel);
    }
    if (p === from) return;
    const [item] = state.queue.splice(from, 1);
    state.queue.splice(p, 0, item);
    let i = 0;
    for (const r of seq) {
      r.dataset.index = i;
      const num = r.querySelector('.q-idx');
      if (num && !r.classList.contains('on')) num.textContent = i + 1;
      i++;
    }
    invoke('playlist_reorder', { from, to: p }).catch((err) => {
      toast(`排序失败:${err}`);
      refreshQueue();
    });
  };
  handle.addEventListener('pointermove', onMove);
  handle.addEventListener('pointerup', finish);
  handle.addEventListener('pointercancel', finish);
  // 拖动中滚轮/滚动条变化也保持行钉在光标下(自动滚动已走 update,幂等)
  list.addEventListener('scroll', onScroll);
}

function updateQueueSentinel(list) {
  list.querySelector('.queue-more')?.remove();
  if (queueRendered < state.queue.length) {
    const more = document.createElement('div');
    more.className = 'queue-more dim';
    more.textContent = `↓ 滚动加载更多(已显示 ${queueRendered} / ${state.queue.length})`;
    list.appendChild(more);
    queueScrollObserver.observe(more);
    $('queue-foot').textContent = `已显示 ${queueRendered} / ${state.queue.length}`;
  } else {
    $('queue-foot').textContent = '';
  }
}

function renderQueueMore() {
  const list = $('queue-list');
  if (queueRendered >= state.queue.length) return;
  const to = Math.min(state.queue.length, queueRendered + QUEUE_BATCH);
  appendQueueRows(list, queueRendered, to);
  queueRendered = to;
  updateQueueSentinel(list);
}

function renderQueue() {

  const list = $('queue-list');
  const items = state.queue;
  list.classList.toggle('random', state.libMode === 'random');
  list.classList.toggle('selecting', !!state.qsel);
  // 多选模式:丢弃已不存在的 qid(移除/清空后集合残留)
  if (state.qsel) {
    const live = new Set(items.map((t) => t.qid));
    for (const q of [...state.qsel]) {
      if (!live.has(q)) state.qsel.delete(q);
    }
    updateSelBar();
  }
  $('queue-count').textContent = items.length;
  const totalMs = items.reduce((s, t) => s + (t.lengthMs || 0), 0);
  $('queue-summary').textContent = items.length
    ? `${items.length} 首 · 约 ${fmtTotal(totalMs)}`
    : '队列空闲';
  queueScrollObserver.disconnect();
  list.innerHTML = '';
  if (!items.length) {
    list.innerHTML =
      '<div class="queue-empty">播放列表为空 —— 切到「曲库」过滤后「添加到播放列表」,或双击直接播放</div>';
    $('queue-foot').textContent = '';
    return;
  }
  // 首批:开头 → 当前曲目(再带一段余量),当前曲不可见时用户滚动即续载
  const curIdx = items.findIndex((t) => t.current);
  const initial = Math.max(QUEUE_BATCH, (curIdx >= 0 ? curIdx : 0) + QUEUE_BATCH / 2);
  const to = Math.min(items.length, initial);
  const keepScroll = list.scrollTop;
  appendQueueRows(list, 0, to);
  queueRendered = to;
  updateQueueSentinel(list);
  if (state.qsel) {
    // 多选重渲染不打断操作:保持滚动位置,不吸滚到当前曲
    list.scrollTop = keepScroll;
    return;
  }
  const cur = list.querySelector('.queue-row.on');
  if (cur) cur.scrollIntoView({ block: 'center' });
}

// 清空播放列表:先弹确认(防误触,清了无法撤销)
$('queue-clear').addEventListener('click', () => {
  const n = state.queue?.length ?? 0;
  if (!n) {
    toast('播放列表已是空的');
    return;
  }
  $('clear-modal-text').textContent = `确定清空全部 ${n} 首吗?此操作无法撤销。`;
  $('clear-modal').hidden = false;
  $('clear-modal-ok').focus();
});
async function confirmClearQueue() {
  $('clear-modal').hidden = true;
  try {
    await invoke('playlist_clear');
    refreshQueue();
    toast('播放列表已清空');
  } catch (e) {
    toast(`${e}`);
  }
}
$('clear-modal-ok').addEventListener('click', confirmClearQueue);
$('clear-modal-cancel').addEventListener('click', () => {
  $('clear-modal').hidden = true;
});

// ---- mods 对话框(加入播放列表 / 编辑条目)----
// 上下文:{ mode:'add', setId, initialSha2? } | { mode:'batch', count } |
// { mode:'edit', qid, setId, sha2, mods, title }(sha2 非空 = 锁定谱面)
let modsCtx = null;

function setModsUi(bits) {
  document.querySelectorAll('#mods-modal .mod-btn').forEach((b) => {
    b.classList.toggle('on', (bits & MOD_BITS[b.dataset.mod]) !== 0);
  });
}
function readModsUi() {
  let bits = 0;
  document.querySelectorAll('#mods-modal .mod-btn.on').forEach((b) => {
    bits |= MOD_BITS[b.dataset.mod];
  });
  return bits;
}

function openModsModal(ctx) {
  modsCtx = ctx;
  const isEdit = ctx.mode === 'edit';
  const isBatch = ctx.mode === 'batch';
  const isMulti = ctx.mode === 'multi';
  // 单曲加入与批量同款:不做难度下拉,只留「导入选中难度」勾选
  const isAdd = ctx.mode === 'add';
  const set = ctx.setId ? state.lib?.find((s) => s.id === ctx.setId) : null;
  $('mods-title').textContent = isEdit ? '修改条目' : isMulti ? '修改 mods' : '加入播放列表';
  $('mods-sub').textContent = isMulti
    ? `所选 ${ctx.count} 个条目将统一应用`
    : isBatch
      ? `${ctx.count} 个谱面集将加入播放列表`
      : (ctx.title || (set ? (set.titleUnicode || set.title) : '') || '');
  setModsUi(ctx.mods || 0);
  // 难度策略行常驻(单曲/批量):最难/最容易/自定义(内联星级输入)。
  // 难度 chip 右键时置顶插入「所选难度」项;编辑(逐难度下拉)与
  // 多选不显示本行。
  const showImportDiff = isAdd || isBatch;
  $('mods-policy-row').hidden = !showImportDiff;
  if (showImportDiff) {
    const psel = $('mods-policy');
    psel.innerHTML = '';
    let pendingCollOption = null;
    // 显式难度项置顶,默认优先选中:
    // 1) 难度 chip 右键 = 「所选难度」;
    // 2) 收藏夹筛选激活 = 「当前难度」(收藏命中的那档,特权项:单
    //    命中即该档;多命中全部列出,默认选策略在命中内挑的那档)
    let defaultVal = policyMode;
    // 批量:收藏筛选激活 → 置顶「当前难度」(每集锁收藏命中的那档;
    // 命中多个难度 fallback 到用户设置的默认难度挑最相近)
    if (isBatch && $('lib-collection').value) {
      const opt = document.createElement('option');
      opt.value = 'coll';
      opt.textContent = '当前难度(收藏命中)';
      opt.title = '每个谱面集锁定收藏命中的那档难度;命中多档时按默认难度挑最相近';
      // 占位:插到策略三选之前(下方 append)
      pendingCollOption = opt;
      defaultVal = 'coll';
    }
    const explicit = [];
    if (ctx.initialSha2 && set?.beatmaps?.some((b) => b.sha2 === ctx.initialSha2)) {
      const b = set.beatmaps.find((x) => x.sha2 === ctx.initialSha2);
      explicit.push({ sha2: b.sha2, label: `所选难度:${b.name}${b.starRating > 0 ? ` ★${b.starRating.toFixed(2)}` : ''}` });
    } else if (isAdd && set) {
      const cand = collFilterDiffs(set);
      if (cand?.length) {
        // 多命中:全部列出,默认选中策略挑的那档
        for (const b of cand) {
          explicit.push({ sha2: b.sha2, label: `当前难度:${b.name}${b.starRating > 0 ? ` ★${b.starRating.toFixed(2)}` : ''}` });
        }
      }
    }
    if (explicit.length === 1) {
      defaultVal = `sha2:${explicit[0].sha2}`;
    } else if (explicit.length > 1) {
      // 多命中:默认 = 记忆策略在命中难度内挑的那档(仍是"当前难度")
      defaultVal = `sha2:${pickDiffLocal(explicit.map((e) => {
        const b = set.beatmaps.find((x) => x.sha2 === e.sha2);
        return b;
      }), policyTarget()).sha2}`;
    }
    for (const e of explicit) {
      const opt = document.createElement('option');
      opt.value = `sha2:${e.sha2}`;
      opt.textContent = e.label;
      psel.appendChild(opt);
    }
    if (pendingCollOption) psel.appendChild(pendingCollOption);
    for (const [v, label] of [['hardest', '最难'], ['easiest', '最容易'], ['custom', '自定义星级']]) {
      const opt = document.createElement('option');
      opt.value = v;
      opt.textContent = label;
      psel.appendChild(opt);
    }
    psel.value = defaultVal;
    $('mods-star-input').value = targetStar != null ? String(targetStar) : '';
    $('mods-star-input').hidden = psel.value !== 'custom';
  }
  // 编辑条目的预选难度:锁定的难度;谱面集级条目(历史)按第一档
  const initialSha2 = ctx.initialSha2 ?? ctx.sha2 ?? null;
  // 难度下拉(谱面数据未就绪时退化为锁定隐藏,保存仍带 initialSha2)
  const sel = $('mods-diff');
  if (isBatch || isMulti || !set || !set.beatmaps?.length) {
    // 批量跨集无下拉;多选同;曲库未就绪时无难度数据
    $('mods-diff-row').hidden = true;
  } else {
    // 填充难度下拉:编辑锁定态显示;单曲加入取消勾选「导入选中难度」
    // 时显示(手动挑一档);批量不显示
    sel.innerHTML = '';
    for (const b of [...set.beatmaps].sort((a, x) => a.starRating - x.starRating)) {
      const opt = document.createElement('option');
      opt.value = b.sha2;
      opt.textContent = `${b.name}${b.starRating > 0 ? ` ★${b.starRating.toFixed(2)}` : ''}`;
      sel.appendChild(opt);
    }
    const sorted = [...set.beatmaps].sort((a, x) => a.starRating - x.starRating);
    sel.value = sorted.some((b) => b.sha2 === initialSha2) ? initialSha2 : (sorted[0]?.sha2 ?? '');
    // 逐难度下拉:仅编辑模式显示(解锁走多选「清除设置」)
    $('mods-diff-row').hidden = !isEdit;
  }
  $('mods-ok').textContent = isEdit ? '保存' : isMulti ? '应用' : '加入';
  $('mods-modal').hidden = false;
}

function closeModsModal() {
  $('mods-modal').hidden = true;
  modsCtx = null;
}

// mod 按钮:toggle + 互斥(HR↔EZ;DT/HT/NC 三选一)
document.querySelector('#mods-modal .mods-row').addEventListener('click', (e) => {
  const b = e.target.closest('.mod-btn');
  if (!b) return;
  const key = b.dataset.mod;
  if (!b.classList.contains('on') && MOD_EXCL[key]) {
    for (const other of MOD_EXCL[key]) {
      document.querySelector(`#mods-modal .mod-btn[data-mod="${other}"]`)?.classList.remove('on');
    }
  }
  b.classList.toggle('on');
});
// 策略下拉联动:自定义 = 显示内联星级输入框(不再弹窗)
$('mods-policy').addEventListener('change', () => {
  $('mods-star-input').hidden = $('mods-policy').value !== 'custom';
  if (!$('mods-star-input').hidden) $('mods-star-input').focus();
});
$('mods-cancel').addEventListener('click', closeModsModal);
$('mods-ok').addEventListener('click', async () => {
  const ctx = modsCtx;
  if (!ctx) return closeModsModal();
  const bits = readModsUi();
  // 编辑 = 覆盖 mods 与锁定难度(恒指定难度,无谱面集选项)
  const sha2 = ctx.mode === 'edit' ? ($('mods-diff').value || null) : null;
  closeModsModal();
  try {
    if (ctx.mode === 'multi') {
      await invoke('queue_set_mods', { qids: ctx.qids, mods: bits });
      toast(`已将所选 ${ctx.qids.length} 个条目的 mods 统一为${bits ? ' ' + describeMods(bits) : '无'}`);
    } else if (ctx.mode === 'batch') {
      const { target } = readPolicySelection(null);
      const n = await invoke('playlist_add_batch', {
        items: filteredSets().map((s) => {
          // 收藏筛选激活:只在命中的难度里挑(单命中=该档,多命中=策略
          // 最相近);未激活 = 后端全集按策略挑
          const sha2 = collPickSha2(s, target);
          return { setId: s.id, sha2, mods: bits, target: sha2 ? null : target };
        }),
      });
      toast(n > 0 ? `已添加 ${n} 个谱面集到播放列表` : '没有新条目(可能已在列表中)');
    } else if (ctx.mode === 'edit') {
      await invoke('queue_set_entry', { qid: ctx.qid, mods: bits, sha2 });
      toast('条目已更新');
    } else {
      // 难度 = 策略行所选:「所选难度」(chip)/ 最难 / 最容易 / 自定义
      // 星级(输入框);收藏筛选激活时只在命中难度里挑
      const { sha2, target } = readPolicySelection(ctx.setId);
      const added = await invoke('playlist_add', { setId: ctx.setId, sha2, mods: bits, target });
      toast(added ? '已添加到播放列表' : '已替换现有条目参数');
    }
    refreshQueue();
  } catch (e) {
    toast(`${ctx.mode === 'edit' ? '保存失败' : '加入失败'}:${e}`);
  }
});

// ---- 播放列表多选:选择 → 右键 → 清除设置 ----
// 条目本体恒为谱面集;mods 与难度锁定都是可清除的状态覆盖,
// 清除后难度回到目标星级(最难/最简单/自定义)解析。
function setQueueSelect(on) {
  state.qsel = on ? new Set() : null;
  $('queue-selbar').hidden = !on;
  $('queue-normalbar').hidden = on;
  closeQSelMenu();
  updateSelBar();
  renderQueue();
}

function updateSelBar() {
  const n = state.qsel?.size ?? 0;
  const total = state.queue?.length ?? 0;
  $('qsel-info').textContent = n ? `已选 ${n} 首` : '点击选择 · 右键批量操作';
  $('qsel-all').textContent = total > 0 && n >= total ? '全不选' : '全选';
}

function toggleQSel(qid) {
  if (!state.qsel) return;
  if (state.qsel.has(qid)) state.qsel.delete(qid);
  else state.qsel.add(qid);
  updateSelBar();
  renderQueue();
}

$('queue-select').addEventListener('click', () => setQueueSelect(true));
$('qsel-done').addEventListener('click', () => setQueueSelect(false));
$('qsel-all').addEventListener('click', () => {
  if (!state.qsel) return;
  const total = state.queue?.length ?? 0;
  state.qsel = total > 0 && state.qsel.size >= total
    ? new Set()
    : new Set(state.queue.map((t) => t.qid));
  updateSelBar();
  renderQueue();
});

// 多选右键菜单:跟随鼠标的浮层;点击外部 / Escape 关闭
function openQSelMenu(x, y, page = 'main') {
  const menu = $('qsel-menu');
  if (page === 'main') {
    const n = state.qsel?.size ?? 0;
    if (!n) return closeQSelMenu();
    $('qsel-menu-mods').textContent = `修改 mods…(${n} 首)`;
  }
  menu.hidden = false;
  showQSelPage(page);
  const r = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(8, Math.min(x, window.innerWidth - r.width - 8))}px`;
  menu.style.top = `${Math.max(8, Math.min(y, window.innerHeight - r.height - 8))}px`;
}
function closeQSelMenu() {
  $('qsel-menu').hidden = true;
  showQSelPage('main');
}
// 常态页「修改…」→ 编辑对话框(mods + 锁定难度)
$('qsel-menu-edit-open').addEventListener('click', () => {
  const item = qeditCtx;
  closeQSelMenu();
  if (!item) return;
  // sha2 = 当前难度(锁定条目 = 锁定的那档;谱面集级 = 正在解析
  // 播放的那档),编辑对话框预选它,用户可任意改选
  openModsModal({
    mode: 'edit',
    qid: item.qid,
    setId: item.setId,
    sha2: item.sha2 || null,
    mods: item.mods,
    title: item.title,
  });
});
document.addEventListener('click', (ev) => {
  if (!ev.target.closest('#qsel-menu')) closeQSelMenu();
});
window.addEventListener('blur', closeQSelMenu);
$('qsel-menu-mods').addEventListener('click', () => {
  closeQSelMenu();
  const qids = [...(state.qsel ?? [])];
  if (!qids.length) return;
  // 初值:所选条目 mods 全部一致则显示该值,否则从空白开始
  const items = state.queue.filter((t) => qids.includes(t.qid));
  const first = items[0]?.mods ?? 0;
  const same = items.every((t) => t.mods === first);
  openModsModal({ mode: 'multi', qids, count: qids.length, mods: same ? first : 0 });
});

// 右键:曲库行/难度 chip = 以该集(该难度)加入;播放列表条目 = 编辑 mods
$('lib-list').addEventListener('contextmenu', (ev) => {
  const chip = ev.target.closest('.chip');
  const el = chip ?? ev.target.closest('.lib-row-head');
  if (!el?.dataset.set) return;
  ev.preventDefault();
  openModsModal({ mode: 'add', setId: el.dataset.set, initialSha2: chip?.dataset.sha2 ?? null });
});
$('queue-list').addEventListener('contextmenu', (ev) => {
  const row = ev.target.closest('.queue-row');
  if (!row?.dataset.qid) return;
  const item = state.queue?.find((t) => String(t.qid) === row.dataset.qid);
  if (!item) return;
  ev.preventDefault();
  if (state.qsel) {
    // 多选模式:右键未选中的行先加入选中(资源管理器语义),
    // 菜单作用于整个选中集
    if (!state.qsel.has(item.qid)) {
      state.qsel.add(item.qid);
      row.classList.add('selected');
      row.querySelector('.q-check')?.classList.add('on');
      updateSelBar();
    }
    openQSelMenu(ev.clientX, ev.clientY, 'main');
    return;
  }
  // 常态:二级菜单(「修改…」再进对话框)
  qeditCtx = item;
  openQSelMenu(ev.clientX, ev.clientY, 'edit');
});
window.addEventListener('keydown', (ev) => {
  if (ev.key !== 'Escape') return;
  if (!$('clear-modal').hidden) $('clear-modal').hidden = true;
  if (!$('mods-modal').hidden) closeModsModal();
  if (!$('qsel-menu').hidden) closeQSelMenu();
});

// 封面懒加载观察器(视口为根:曲库列表与播放列表共用)
const coverObserver = new IntersectionObserver(async (entries) => {
  for (const en of entries) {
    if (!en.isIntersecting) continue;
    coverObserver.unobserve(en.target);
    const id = en.target.dataset.cover;
    try {
      const url = await invoke('set_cover', { setId: id });
      if (url) {
        en.target.src = url;
      }
      en.target.classList.remove('loading');
    } catch {
      en.target.classList.remove('loading');
    }
  }
}, { rootMargin: '300px' });

/// 取封面 data URL(带简单内存缓存)。
const coverMem = new Map();
async function coverOf(setId) {
  if (!setId) return null;
  if (coverMem.has(setId)) return coverMem.get(setId);
  try {
    const url = await invoke('set_cover', { setId });
    coverMem.set(setId, url);
    if (coverMem.size > 120) coverMem.clear();
    return url;
  } catch {
    return null;
  }
}

async function refreshNowPlaying() {
  // 优先后端播放真相(lazer_set/lazer_sha2,含手动难度);取不到再退
  // 播放列表当前条目(队列恢复早期曲库未就绪等)
  if (!state.nowTrack) {
    try {
      const cur = await invoke('playing_track').catch(() => null)
        ?? await invoke('current_track').catch(() => null);
      if (cur?.setId) state.nowTrack = { setId: cur.setId, sha2: cur.sha2 ?? null, qid: cur.qid };
    } catch { /* 忽略 */ }
  }
  if (!state.nowTrack) return;
  const info = describeTrack(state.nowTrack.setId, state.nowTrack.sha2);
  if (info) setNowPlaying(info.title, subOf(info));
  setNowPlayingCover(state.nowTrack.setId);
  updateDiffSelect(state.nowTrack.setId, state.nowTrack.sha2);
}


/// 右侧当前应显示的曲目:有预览显示预览,否则显示正在播放的
function sideTarget() {
  if (state.preview) return { setId: state.preview.setId, sha2: state.preview.sha2 };
  return { setId: state.nowTrack?.setId ?? null, sha2: state.nowTrack?.sha2 ?? null };
}

/// 右侧(大封面 + 标题 + 信息卡)渲染;seq 防止快速连点时旧响应覆盖新内容
let sideRenderSeq = 0;
async function renderSide(setId, sha2) {
  const seq = ++sideRenderSeq;
  const big = $('big-cover');
  const empty = $('big-cover-empty');
  const cap = $('cover-cap');
  const url = await coverOf(setId);
  if (seq !== sideRenderSeq) return;
  if (!url) {
    big.hidden = true;
    big.src = '';
    empty.hidden = false;
    cap.hidden = true;
    renderBeatmapInfo(null);
    return;
  }
  big.src = url;
  big.hidden = false;
  empty.hidden = true;
  const info = describeTrack(setId, sha2);
  $('cover-title').textContent = info?.title ?? '';
  renderCoverMeta(info);
  cap.hidden = false;
  renderBeatmapInfo(setId, sha2);
}

/// 单击行:右侧预览该谱面;再点同一条切回正在播放的。播放条不受影响。
function previewSet(setId, sha2) {
  const same =
    state.preview &&
    state.preview.setId === setId &&
    (state.preview.sha2 ?? null) === (sha2 ?? null);
  state.preview = same ? null : { setId, sha2: sha2 ?? null };
  // 行高亮直接切类(整列表重渲染会跳滚动位置)
  for (const list of [$('lib-list'), $('queue-list')]) {
    list.querySelectorAll('.previewing').forEach((el) => el.classList.remove('previewing'));
    if (!same) {
      list.querySelectorAll(`[data-set="${setId}"]`).forEach((el) => el.classList.add('previewing'));
    }
  }
  const t = sideTarget();
  renderSide(t.setId, t.sha2);
}

/// 播放条小封面 + 右侧(预览优先)。正在播放变化时调用。
async function setNowPlayingCover(setId) {
  const img = $('pb-cover');
  const url = await coverOf(setId);
  img.src = url ?? '';
  img.hidden = !url;
  const t = sideTarget();
  await renderSide(t.setId, t.sha2);
}

// 点播放条封面:右侧切回正在播放的曲目(清除预览选中态)
$('pb-cover').addEventListener('click', () => {
  if (!state.preview) return;
  state.preview = null;
  for (const list of [$('lib-list'), $('queue-list')]) {
    list.querySelectorAll('.previewing').forEach((el) => el.classList.remove('previewing'));
  }
  const t = sideTarget();
  renderSide(t.setId, t.sha2);
});

// ---- 谱面信息卡(封面下方):AR/CS/OD/HP/BPM/时长/谱师/bid + tags;
//      条目单击即复制 ----

/// 数值显示:最多两位小数,去掉多余的 0(9 → "9",8.5 → "8.5")。
const fmtNum = (v) => String(Math.round(v * 100) / 100);

async function copyText(text) {
  const preview = text.length > 36 ? text.slice(0, 36) + '…' : text;
  const show = () => toast(`已复制:${preview}`);
  try {
    await navigator.clipboard.writeText(text);
    show();
    return;
  } catch { /* WebView 未聚焦等,降级 execCommand */ }
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.cssText = 'position:fixed;opacity:0';
  document.body.appendChild(ta);
  ta.select();
  try {
    if (document.execCommand('copy')) show();
    else toast('复制失败');
  } catch {
    toast('复制失败');
  }
  ta.remove();
}

/// 信息条目(`AR 9.6` 一对);单击复制值(手动选中文本时不打扰)。
/// payload 可选:复制的实际内容与显示值不同时使用(如 bid 复制完整链接)。
function makeInfoItem(label, value, span, payload) {
  const item = document.createElement('div');
  item.className = 'info-item' + (span > 1 ? ` span${span}` : '');
  const l = document.createElement('span');
  l.className = 'l';
  l.textContent = label;
  const v = document.createElement('span');
  v.className = 'v';
  v.textContent = value;
  item.append(l, v);
  item.addEventListener('click', () => {
    if (String(getSelection())) return; // 正在选文字,交给 Ctrl+C
    copyText(payload ?? value);
  });
  return item;
}

/// 渲染信息卡:指定难度(sha2 匹配,缺省取最难);值为 0/空的字段不显示。
function renderBeatmapInfo(setId, sha2) {
  const card = $('info-card');
  const grid = $('info-grid');
  grid.innerHTML = '';
  const tagsEl = $('info-tags');
  tagsEl.innerHTML = '';
  tagsEl.hidden = true;
  const set = setId ? state.lib?.find((s) => s.id === setId) : null;
  if (!set) {
    card.hidden = true;
    return;
  }
  let b = sha2 ? set.beatmaps.find((x) => x.sha2 === sha2) : null;
  if (!b && set.beatmaps.length) {
    b = set.beatmaps.reduce((m, x) => (x.starRating > m.starRating ? x : m), set.beatmaps[0]);
  }
  const frag = document.createDocumentFragment();
  if (b) {
    if (b.ar > 0) frag.appendChild(makeInfoItem('AR', fmtNum(b.ar)));
    if (b.cs > 0) frag.appendChild(makeInfoItem('CS', fmtNum(b.cs)));
    if (b.od > 0) frag.appendChild(makeInfoItem('OD', fmtNum(b.od)));
    if (b.hp > 0) frag.appendChild(makeInfoItem('HP', fmtNum(b.hp)));
    if (b.bpm > 0) frag.appendChild(makeInfoItem('BPM', fmtNum(b.bpm)));
    if (b.lengthMs > 0) frag.appendChild(makeInfoItem('时长', fmt(b.lengthMs)));
    if (set.creator) frag.appendChild(makeInfoItem('谱师', set.creator));
    if (b.onlineId > 0) {
      frag.appendChild(makeInfoItem('bid', String(b.onlineId), 1, `https://osu.ppy.sh/b/${b.onlineId}`));
    }
  }
  if (set.source) frag.appendChild(makeInfoItem('来源', set.source, 4));
  grid.appendChild(frag);
  const tags = set.tags.split(/\s+/).filter(Boolean);
  if (tags.length) {
    for (const t of tags) {
      const chip = document.createElement('span');
      chip.className = 'tag';
      chip.textContent = t;
      chip.addEventListener('click', () => {
        if (String(getSelection())) return;
        copyText(t);
      });
      tagsEl.appendChild(chip);
    }
    tagsEl.hidden = false;
  }
  card.hidden = !grid.childElementCount && tagsEl.hidden;
}

// 封面标题/元信息行均可单击复制(整行一起 = 复制那一行的内容)
$('cover-title').addEventListener('click', () => {
  const t = $('cover-title').textContent;
  if (t && !String(getSelection())) copyText(t);
});
$('cover-meta').addEventListener('click', (ev) => {
  const t = ev.target.textContent;
  if (t && !String(getSelection())) copyText(t);
});

function esc(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

async function playSet(setId, sha2) {
  try {
    await invoke('library_play', { setId, sha2: sha2 ?? null });
    state.nowTrack = { setId, sha2: sha2 ?? null };
    setNowPlayingCover(setId);
    updateDiffSelect(setId, sha2 ?? null);
    renderLibList();
    refreshQueue();
  } catch (e) {
    toast(`播放失败：${e}`);
  }
}

// ---- 初始化:载入保存的设置 ----
// 标题旁版本号(tauri.conf 的 version,与应用打包版本同源)
window.__TAURI__.app?.getVersion?.()
  .then((v) => {
    $('app-ver').textContent = `v${v}`;
    $('about-ver').textContent = `v${v}`;
  })
  .catch(() => {});
(async () => {
  try {
    const s = await invoke('get_settings');
    state.source = s.source ?? 'lazer';
    $('loop').checked = s.loop_playback ?? false;
    applySpeedUi(s.speed ?? 1);
    $('autostart').checked = !!s.autostart;
    $('log-rec').checked = !!s.log_enabled;
    // 播放条音量即总音量(音乐 + 音效主增益)
    const master = s.master_volume ?? 1;
    $('pb-vol').value = String(master);
    $('pb-vol-val').textContent = `${Math.round(master * 100)}%`;
    state.lastVol = master;
    $('hitsound').checked = s.hitsound ?? true;
    $('fade').checked = s.fadeAudio ?? true;
    $('vol').value = String(s.volume ?? 0.6);
    $('vol-val').textContent = `${Math.round((s.volume ?? 0.6) * 100)}%`;
    $('hits-volume').value = String(s.hits_volume ?? 0.8);
    $('hits-volume-val').textContent = `${Math.round((s.hits_volume ?? 0.8) * 100)}%`;
    $('hits-volume').disabled = !$('hitsound').checked;
    $('offset').value = String(s.audio_offset_ms ?? 0);
    $('offset-val').textContent = `${Math.round(s.audio_offset_ms ?? 0)}ms`;
    $('hud').checked = !!s.hud;
    $('pp').checked = !!s.pp;
    $('reduce-anim').checked = !!s.reduce_anim;
    $('break-lighten').checked = !!s.break_lighten;
    $('cursor').checked = s.cursor ?? true;
    const csz = s.cursor_size ?? 1;
    $('cursor-size').value = String(csz);
    $('cursor-size-val').textContent = `${Math.round(csz * 100)}%`;
    $('render-mode').value = ['always', 'autopause', 'fs_pause', 'fs_sleep', 'off'].includes(s.render_mode) ? s.render_mode : 'autopause';
    $('bg-opacity').value = String(s.bg_opacity ?? 0.7);
    $('bg-opacity-val').textContent = `${Math.round((s.bg_opacity ?? 0.7) * 100)}%`;
    $('hidden').checked = !!s.hidden;
    $('gameplay-hidden').checked = !!s.gameplayHidden;
    $('sb').checked = s.storyboard ?? true;
    $('bm-hits').checked = s.beatmap_hitsounds ?? true;
    $('force-colours').checked = !!s.force_skin_colours;
    $('fps').value = String(s.fps ?? 0);
    $('close-action').value = s.close_action ?? '';
    ffmpegBins.ffmpeg = s.ffmpeg ?? null;
    ffmpegBins.ffprobe = s.ffprobe ?? null;
    updateBinsUI();
    refreshBinPaths();
    updateDirUI('lazer-dir', s.lazer_dir);
    updateDirUI('stable-dir', s.stable_dir);
    starRange.lo = s.star_min > 0 ? Math.min(s.star_min, STAR_MAX - 0.5) : 0;
    starRange.hi = null;
    srRender();
    // 默认难度值(多选「设置难度」/导入自选记住的)
    const tv = s.target_star;
    targetStar = tv != null && tv > 0 ? tv : null;
    policyMode = tv == null ? 'hardest' : tv < 0 ? 'easiest' : 'custom';
    state.dblAdd = !!s.dblclick_add;
    $('dblclick-add').checked = state.dblAdd;
    state.quickAdd = s.quick_add ?? true;
    $('quick-add').checked = state.quickAdd;
    state.quickMods = s.quick_mods ?? 0;
    applyQaModsUi(state.quickMods);
    syncQaPolicyUi();
    state.libMode = s.play_mode === 'random' ? 'random' : 'list_order';
    setModeLabel(state.libMode);
    state.libCollection = s.collection ?? '';
  } catch (e) {
    toast(`读取设置失败：${e}`);
  }

  await listen('wall://event', ({ payload }) => onEvent(payload));
  await listen('wall://track', ({ payload }) => {
    // 自动切歌 / 启动恢复 / 难度切换时父进程广播当前曲目(难度已解析)
    if (payload?.setId) {
      state.nowTrack = { setId: payload.setId, sha2: payload.sha2 ?? null, qid: payload.qid };
      const info = describeTrack(payload.setId, payload.sha2);
      if (info) setNowPlaying(info.title, subOf(info));
      setNowPlayingCover(payload.setId);
      updateDiffSelect(payload.setId, payload.sha2);
      renderLibList();
      refreshQueue();
    }
  });

  // 播放列表秒显:不等曲库解析,先按持久化快照渲染(后端 reload_saved
  // 异步灌回持久化列表后再刷一次)
  refreshQueue();
  setTimeout(refreshQueue, 600);
  loadLibrary(false).then(async () => {
    // 曲库就绪后:恢复正在播放标题/封面,并重取播放列表精化
    //(难度星级/封面/最新标题;后端 reload_saved 可能刚灌回列表)
    await refreshNowPlaying();
    await refreshQueue();
  });

  // 兜底轮询:补上监听前丢掉的状态
  setInterval(async () => {
    try {
      const ev = await invoke('get_status');
      if (ev) onEvent(ev);
    } catch { /* 忽略 */ }
  }, 1500);
})();

// ---- 皮肤:已安装皮肤下拉(lazer/stable 解析列表,无手动导入) ----
async function refreshSkinSelect(current) {
  const sel = $('skin-select');
  let cur = current;
  if (cur == null) {
    try { cur = (await invoke('get_settings')).skin ?? ''; } catch { cur = ''; }
  }
  let choices = [];
  try { choices = await invoke('skin_list'); } catch { /* 无 */ }
  const groups = new Map();
  for (const c of choices) {
    if (!groups.has(c.source)) groups.set(c.source, []);
    groups.get(c.source).push(c);
  }
  sel.innerHTML = '<option value="">默认</option>';
  const labels = { lazer: 'osu!lazer', stable: 'osu!stable' };
  for (const [src, items] of groups) {
    const og = document.createElement('optgroup');
    og.label = labels[src] ?? src;
    for (const c of items) {
      const o = document.createElement('option');
      o.value = c.path;
      o.textContent = c.name;
      og.appendChild(o);
    }
    sel.appendChild(og);
  }
  // 只认解析出的已安装皮肤:设置里残留的失效路径(旧导入缓存等)
  // 静默清掉,回默认皮肤
  if (cur && ![...sel.options].some((o) => o.value === cur)) {
    sel.value = '';
    invoke('set_skin', { path: null }).catch(() => {});
  } else {
    sel.value = cur;
  }
}
refreshSkinSelect();
$('skin-select').addEventListener('change', async () => {
  const v = $('skin-select').value;
  try {
    await invoke('set_skin', { path: v || null });
    toast(v ? `已应用皮肤:${$('skin-select').selectedOptions[0].textContent}` : '已恢复默认');
  } catch (e) {
    toast(`${e}`);
    refreshSkinSelect();
  }
});
$('force-colours').addEventListener('change', () => {
  invoke('set_force_skin_colours', { on: $('force-colours').checked }).catch((e) => toast(`${e}`));
});

$('loop').addEventListener('change', () => {
  invoke('set_loop', { on: $('loop').checked }).catch(() => {});
});

// 播放速度:播放条按钮弹出预设菜单,点击即下发(后端持久化并实时
// 变速不变调;选中态由 Status 事件回推,这里只发命令不抢状态)
$('pb-speed').addEventListener('click', () => {
  $('speed-pop').classList.toggle('open');
});
$('speed-pop').addEventListener('click', (e) => {
  const b = e.target.closest('.seg-btn');
  if (!b) return;
  $('speed-pop').classList.remove('open');
  invoke('set_speed', { x: Number(b.dataset.speed) }).catch((err) => toast(`${err}`));
});
document.addEventListener('click', (ev) => {
  if (!ev.target.closest('.speed-wrap')) $('speed-pop').classList.remove('open');
});

// 双击曲库行加入播放列表(默认关 = 双击立即播放)
$('dblclick-add').addEventListener('change', () => {
  state.dblAdd = $('dblclick-add').checked;
  invoke('set_dblclick_add', { on: state.dblAdd }).catch(() => {});
});

// 默认导入选项:单击封面快速导入 vs 弹完整对话框
$('quick-add').addEventListener('change', () => {
  state.quickAdd = $('quick-add').checked;
  invoke('set_quick_add', { on: state.quickAdd }).catch((e) => toast(`${e}`));
});

// 快速导入的默认 mods(设置区按钮组,互斥逻辑同导入对话框)
function applyQaModsUi(bits) {
  document.querySelectorAll('#qa-mods .seg-btn').forEach((b) => {
    b.classList.toggle('on', (bits & MOD_BITS[b.dataset.qaMod]) !== 0);
  });
}
$('qa-mods').addEventListener('click', (e) => {
  const b = e.target.closest('.seg-btn');
  if (!b) return;
  const key = b.dataset.qaMod;
  if (!b.classList.contains('on') && MOD_EXCL[key]) {
    for (const other of MOD_EXCL[key]) {
      document.querySelector(`#qa-mods .seg-btn[data-qa-mod="${other}"]`)?.classList.remove('on');
    }
  }
  b.classList.toggle('on');
  let bits = 0;
  document.querySelectorAll('#qa-mods .seg-btn.on').forEach((x) => {
    bits |= MOD_BITS[x.dataset.qaMod];
  });
  state.quickMods = bits;
  invoke('set_quick_mods', { mods: bits }).catch((err) => toast(`${err}`));
});

// 默认难度(= 默认难度值,与导入对话框/多选「设置难度」同源):
// 变更即持久化,UI 双向同步
function syncQaPolicyUi() {
  $('qa-policy').value = policyMode;
  $('qa-star').value = targetStar != null ? String(targetStar) : '';
  $('qa-star').hidden = policyMode !== 'custom';
}
$('qa-policy').addEventListener('change', () => {
  const mode = $('qa-policy').value;
  setPolicy(mode, mode === 'custom' ? targetStar : null);
  syncQaPolicyUi();
  if (mode === 'custom') $('qa-star').focus();
});
function commitQaStar() {
  const v = Number($('qa-star').value.trim());
  if (!Number.isFinite(v) || v <= 0) {
    toast('请输入大于 0 的星级数值');
    return;
  }
  setPolicy('custom', v);
  syncQaPolicyUi();
}
$('qa-star').addEventListener('change', commitQaStar);
$('qa-star').addEventListener('keydown', (ev) => {
  if (ev.key === 'Enter') {
    ev.preventDefault();
    commitQaStar();
  }
});

// ---- 播放器条 ----
$('toggle').addEventListener('click', () => invoke('toggle_play').catch((e) => toast(`${e}`)));
$('prev').addEventListener('click', () => invoke('skip_prev').catch((e) => toast(`${e}`)));
$('next').addEventListener('click', () => invoke('skip_next').catch((e) => toast(`${e}`)));

const pbSeek = $('pb-seek');
pbSeek.addEventListener('input', () => (state.dragging = true));
pbSeek.addEventListener('change', async () => {
  const ms = (Number(pbSeek.value) / 1000) * state.duration;
  state.dragging = false;
  try {
    await invoke('seek', { ms });
    await invoke('resume');
  } catch (e) {
    toast(`${e}`);
  }
});

// 音量弹出层:悬停显示;移开后有 400ms 宽限(从按钮移到滑条不会中途
// 消失),点击外部立即关闭 —— 悬停后可以从容点击/拖动
const volWrap = document.querySelector('.vol-wrap');
let volHideTimer = 0;
function volShow() {
  clearTimeout(volHideTimer);
  $('vol-pop').classList.add('open');
}
function volHideSoon() {
  clearTimeout(volHideTimer);
  volHideTimer = setTimeout(() => $('vol-pop').classList.remove('open'), 400);
}
volWrap.addEventListener('mouseenter', volShow);
volWrap.addEventListener('mouseleave', volHideSoon);
$('vol-pop').addEventListener('mouseenter', volShow);
$('vol-pop').addEventListener('mouseleave', volHideSoon);
document.addEventListener('click', (ev) => {
  if (!volWrap.contains(ev.target)) $('vol-pop').classList.remove('open');
});

// 音量:小喇叭弹出竖向滑条(hover 弹出);点击小喇叭静音/恢复
$('pb-vol').addEventListener('input', () => {
  const v = Number($('pb-vol').value);
  $('pb-vol-val').textContent = `${Math.round(v * 100)}%`;
  state.muted = v === 0;
  if (v > 0) state.lastVol = v;
});
$('pb-vol').addEventListener('change', () => {
  invoke('set_master_volume', { v: Number($('pb-vol').value) }).catch(() => {});
});
$('pb-vol-btn').addEventListener('click', () => {
  const v = state.muted ? (state.lastVol || 1) : 0;
  state.muted = !state.muted;
  $('pb-vol').value = String(v);
  $('pb-vol-val').textContent = `${Math.round(v * 100)}%`;
  invoke('set_master_volume', { v }).catch(() => {});
});

// 播放模式:底栏按钮循环切换(只改遍历方式,不动列表内容)
function applyMode(mode) {
  state.libMode = mode;
  setModeLabel(mode);
  invoke('playlist_set', { mode, collection: state.libCollection || null })
    .then(() => refreshQueue())
    .catch((e) => toast(`${e}`));
}
$('pb-mode').addEventListener('click', () => {
  const next = MODE_ORDER[(MODE_ORDER.indexOf(state.libMode) + 1) % MODE_ORDER.length];
  applyMode(next);
});
// 收藏夹 = 曲库过滤器(播放列表内容始终由用户显式添加)
$('lib-collection').addEventListener('change', () => {
  state.libCollection = $('lib-collection').value;
  renderLibList();
});

// ---- 曲库交互 ----
let srcPending = null;
$('src-switch').querySelectorAll('button').forEach((btn) => {
  btn.addEventListener('click', () => {
    const src = btn.dataset.src;
    if (src === state.source) return;
    if (state.queue.length === 0) {
      switchSource(src);
      return;
    }
    srcPending = src;
    $('src-modal-target').textContent = src === 'stable' ? 'osu!stable' : 'osu!lazer';
    $('src-modal').hidden = false;
  });
});
$('src-modal-ok').addEventListener('click', () => {
  $('src-modal').hidden = true;
  const src = srcPending;
  srcPending = null;
  if (src) switchSource(src);
});
$('src-modal-cancel').addEventListener('click', () => {
  $('src-modal').hidden = true;
  srcPending = null;
});
$('lib-sort').addEventListener('change', () => {
  libSort.key = $('lib-sort').value;
  renderLibList();
});
$('lib-sort-dir').addEventListener('click', () => {
  libSort.desc = !libSort.desc;
  $('lib-sort-dir').textContent = libSort.desc ? '↓' : '↑';
  renderLibList();
});
$('lib-refresh').addEventListener('click', () => loadLibrary(true));
$('lib-search').addEventListener('input', renderLibList);
// 星级范围双端滑条(lazer DifficultyRangeSlider:谱色轨道,两端圆钮,
// 右端尽头 = ∞ 不限制;下限/上限互相钳制,拖动实时过滤曲库)
const STAR_MAX = 10;
const starRange = { lo: 0, hi: null, dirty: false };
// 圆钮行程两端内缩量:半宽 17px + 1px 余量;再小钮会压到相邻控件
const SR_PAD = 18;
const srEls = {
  fill: $('sr-fill'), lo: $('sr-nub-lo'), hi: $('sr-nub-hi'),
  tip: $('sr-tip'), track: $('sr-nub-lo').parentElement,
};
function srLabel(which) {
  if (which === 'lo') return starRange.lo > 0 ? `${starRange.lo.toFixed(1)}★` : '0★';
  return starRange.hi === null ? '∞' : `${starRange.hi.toFixed(1)}★`;
}
function srRender() {
  // 轻量唤醒后窗口可能刚重建(clientWidth=0),此时定位算出全 0——
  // 记一次"待重算",tab 可见时再补。
  if (srEls.track.clientWidth < 40) {
    starRange.dirty = true;
    return;
  }
  starRange.dirty = false;
  // 轨道两端各内缩 SR_PAD(圆钮半宽 17px + 1px),定位换算按内缩后的行程
  const w = srEls.track.clientWidth - SR_PAD * 2;
  const pos = (frac) => `calc(${SR_PAD}px + ${(Math.max(0, Math.min(1, frac)) * w).toFixed(1)}px)`;
  const loPct = Math.min(starRange.lo / STAR_MAX, 1);
  const hiPct = starRange.hi === null ? 1 : Math.min(starRange.hi / STAR_MAX, 1);
  srEls.lo.style.left = pos(loPct);
  srEls.hi.style.left = pos(hiPct);
  srEls.fill.style.left = pos(loPct);
  srEls.fill.style.width = `${((hiPct - loPct) * w).toFixed(1)}px`;
  // 范围数值集中在轨道中央显示(钮内不放文字:两钮靠拢会互相遮住)
  srEls.tip.textContent = `${srLabel('lo')} – ${srLabel('hi')}`;
}
function srSet(which, frac) {
  const v = Math.max(0, Math.min(STAR_MAX, frac * STAR_MAX));
  if (which === 'lo') {
    starRange.lo = Math.min(v, (starRange.hi ?? STAR_MAX) - 0.5);
  } else {
    const nv = Math.max(starRange.lo + 0.5, v);
    starRange.hi = nv >= STAR_MAX ? null : nv;
  }
  // 拖动中只更新滑块视觉;列表过滤在松手时统一做(重渲染大列表太贵)
  srRender();
}
function srBind(nub, which) {
  nub.addEventListener('pointerdown', (ev) => {
    ev.preventDefault();
    nub.setPointerCapture(ev.pointerId);
    const move = (e) => {
      const r = srEls.track.getBoundingClientRect();
      const w = r.width - SR_PAD * 2;
      srSet(which, Math.max(0, Math.min(1, (e.clientX - r.left - SR_PAD) / Math.max(w, 1))));
    };
    const up = () => {
      nub.removeEventListener('pointermove', move);
      nub.removeEventListener('pointerup', up);
      renderLibList();
      invoke('set_star_min', { min: starRange.lo }).catch(() => {});
    };
    nub.addEventListener('pointermove', move);
    nub.addEventListener('pointerup', up);
  });
}
srBind(srEls.lo, 'lo');
srBind(srEls.hi, 'hi');
window.addEventListener('resize', () => requestAnimationFrame(srRender));
window.addEventListener('load', () => requestAnimationFrame(srRender));
requestAnimationFrame(srRender);
// ---- 难度统一逻辑:导入即锁定(难度 chip = 该难度;批量 = 单难度
//      直接锁、多难度按默认难度值挑最相近),之后任何条目右键可再改。
//      默认难度值(最难/最简单/自定义星级)无常驻 UI,在多选右键
//      「设置难度」里选择:立即应用到所选条目并记住为新默认(影响
//      之后的批量导入)。 ----
let targetStar = null; // >0 = 自定义星级默认值
let policyMode = 'hardest'; // 记忆的难度策略:hardest / easiest / custom
function policyTarget() {
  return policyMode === 'easiest' ? -1 : policyMode === 'custom' ? targetStar : null;
}
function setPolicy(mode, star) {
  policyMode = mode;
  if (star != null) targetStar = star;
  invoke('set_target_star', { star: policyTarget() }).catch(() => {});
  syncQaPolicyUi();
}
/// 读取导入对话框策略行当前选择 → { sha2, target }(显式难度优先;
/// 自定义星级读内联输入框,合法时记忆为新默认)。`setId` 提供时叠加
/// 收藏筛选命中难度(只在命中的难度里挑)。
function readPolicySelection(setId) {
  const v = $('mods-policy').value;
  if (v === 'coll') {
    // 当前难度(收藏命中):每集锁命中那档;多命中 fallback 按默认
    // 难度挑最相近(collPickSha2 已按此实现),不记忆为新策略
    return { sha2: null, target: policyTarget() };
  }
  if (v.startsWith('sha2:')) return { sha2: v.slice(5), target: null };
  if (v === 'custom') {
    const sv = Number($('mods-star-input').value.trim());
    if (!Number.isFinite(sv) || sv <= 0) {
      return { sha2: null, target: targetStar }; // 输入无效:退回记忆值
    }
    setPolicy('custom', sv);
    const picked = setId ? (() => {
      const set = state.lib?.find((x) => x.id === setId);
      return set ? collPickSha2(set, sv) : null;
    })() : null;
    return { sha2: picked, target: picked ? null : sv };
  }
  setPolicy(v, null);
  const t = v === 'easiest' ? -1 : null;
  const picked = setId ? (() => {
    const set = state.lib?.find((x) => x.id === setId);
    return set ? collPickSha2(set, t) : null;
  })() : null;
  return { sha2: picked, target: picked ? null : t };
}

/// 多选「设置难度」:按策略锁定所选条目的难度(单难度集即那一档),
/// 同时把该策略持久化为新默认(后续导入按它挑最相近)。
async function applyQSelDiff(qids, target) {
  try {
    await invoke('queue_set_diff', { qids, target });
    setPolicy(target == null ? 'hardest' : target < 0 ? 'easiest' : 'custom',
      target != null && target > 0 ? target : null);
    toast(`已设置 ${qids.length} 个条目的难度(并记为默认难度值)`);
    setQueueSelect(false);
    refreshQueue();
  } catch (e) {
    toast(`设置难度失败:${e}`);
  }
}

// 右键菜单页切换:常态(修改入口)/ 多选主页 / 难度子页
let qeditCtx = null; // 常态页「修改…」的目标条目
function showQSelPage(page) {
  $('qsel-menu-edit').hidden = page !== 'edit';
  $('qsel-menu-main').hidden = page !== 'main';
  $('qsel-menu-diffpage').hidden = page !== 'diff';
}
$('qsel-menu-diff').addEventListener('click', () => showQSelPage('diff'));
$('qsel-diff-back').addEventListener('click', () => showQSelPage('main'));
$('qsel-menu-diffpage').addEventListener('click', (e) => {
  const b = e.target.closest('button[data-qdiff]');
  if (!b) return;
  const qids = [...(state.qsel ?? [])];
  closeQSelMenu();
  if (!qids.length) return;
  applyQSelDiff(qids, b.dataset.qdiff === 'easiest' ? -1 : null);
});
// 难度子页内联自定义星级:应用按钮 / 输入框回车
function applyQSelCustomStar() {
  const v = Number($('qsel-star-input').value.trim());
  if (!Number.isFinite(v) || v <= 0) {
    toast('请输入大于 0 的星级数值');
    return;
  }
  const qids = [...(state.qsel ?? [])];
  closeQSelMenu();
  if (qids.length) applyQSelDiff(qids, v);
}
$('qsel-star-apply').addEventListener('click', applyQSelCustomStar);
$('qsel-star-input').addEventListener('keydown', (ev) => {
  if (ev.key === 'Enter') {
    ev.preventDefault();
    applyQSelCustomStar();
  }
});



// ---- 设置 ----
$('hitsound').addEventListener('change', () => {
  $('hits-volume').disabled = !$('hitsound').checked;
  invoke('set_hitsound', { on: $('hitsound').checked }).catch((e) => toast(`${e}`));
});
// BGM 淡入淡出(换曲淡出旧曲、起播淡入;只作用于 BGM,不影响音效)
$('fade').addEventListener('change', () => {
  invoke('set_fade_audio', { on: $('fade').checked }).catch((e) => toast(`${e}`));
});
// 音乐音量:只作用于 BGM(总音量之下的分量,不影响音效)
$('vol').addEventListener('input', () => {
  $('vol-val').textContent = `${Math.round(Number($('vol').value) * 100)}%`;
});
$('vol').addEventListener('change', () => {
  invoke('set_volume', { v: Number($('vol').value) }).catch((e) => toast(`${e}`));
});
// 音效音量:只作用于打击音效(总音量之下的分量,不影响 BGM)
$('hits-volume').addEventListener('input', () => {
  $('hits-volume-val').textContent = `${Math.round(Number($('hits-volume').value) * 100)}%`;
});
$('hits-volume').addEventListener('change', () => {
  invoke('set_hits_volume', { v: Number($('hits-volume').value) }).catch((e) => toast(`${e}`));
});
// 音效偏移:实时生效(与上游 --audio-offset 同语义,正=提前)
$('offset').addEventListener('input', () => {
  const v = Math.round(Number($('offset').value));
  $('offset-val').textContent = `${v > 0 ? '+' : ''}${v}ms`;
});
$('offset').addEventListener('change', () => {
  invoke('set_audio_offset', { ms: Number($('offset').value) }).catch((e) => toast(`${e}`));
});
$('hud').addEventListener('change', () => {
  invoke('set_hud', { on: $('hud').checked }).catch((e) => toast(`${e}`));
});
// PP 计数器(HUD 子项):实时显隐,不重载
$('pp').addEventListener('change', () => {
  invoke('set_pp', { on: $('pp').checked }).catch((e) => toast(`${e}`));
});
// 减少打击动画:命中圆圈 60ms 整体淡出(实时生效)
$('reduce-anim').addEventListener('change', () => {
  invoke('set_reduce_anim', { on: $('reduce-anim').checked }).catch((e) => toast(`${e}`));
});
// 休息段背景变亮(实时生效)
$('break-lighten').addEventListener('change', () => {
  invoke('set_break_lighten', { on: $('break-lighten').checked }).catch((e) => toast(`${e}`));
});
// 光标渲染(实时生效)
$('cursor').addEventListener('change', () => {
  invoke('set_cursor', { on: $('cursor').checked }).catch((e) => toast(`${e}`));
});
// 光标大小(实时生效)
$('cursor-size').addEventListener('input', () => {
  $('cursor-size-val').textContent = `${Math.round(Number($('cursor-size').value) * 100)}%`;
});
$('cursor-size').addEventListener('change', () => {
  invoke('set_cursor_size', { x: Number($('cursor-size').value) }).catch((e) => toast(`${e}`));
});
// 渲染模式:始终渲染 / 全屏时不渲染画面(省电,音乐照常)/ 播放器模式(不渲染)。
// 切换无缝:不重载曲目、不打断音频,仅拆/建渲染会话。
$('render-mode').addEventListener('change', () => {
  invoke('set_render_mode', { mode: $('render-mode').value }).catch((e) => toast(`${e}`));
});
// 背景亮度:实时生效
$('bg-opacity').addEventListener('input', () => {
  $('bg-opacity-val').textContent = `${Math.round(Number($('bg-opacity').value) * 100)}%`;
});
$('bg-opacity').addEventListener('change', () => {
  invoke('set_bg_opacity', { v: Number($('bg-opacity').value) }).catch((e) => toast(`${e}`));
});
// HD(Hidden)视觉:加载期生效,切换会重播当前曲目
$('hidden').addEventListener('change', () => {
  invoke('set_hidden', { on: $('hidden').checked }).catch((e) => toast(`${e}`));
});
// 隐藏游玩画面:实时生效(只渲染背景 + storyboard)
$('gameplay-hidden').addEventListener('change', () => {
  invoke('set_gameplay_hidden', { on: $('gameplay-hidden').checked }).catch((e) => toast(`${e}`));
});
// storyboard / 视频:加载期生效,切换会重播当前曲目
$('sb').addEventListener('change', () => {
  invoke('set_storyboard', { on: $('sb').checked }).catch((e) => toast(`${e}`));
});
// 谱面自带音效(lazer "Beatmap hitsounds"):谱面集采样文件优先于皮肤,
// 加载期生效,切换会重播当前曲目
$('bm-hits').addEventListener('change', () => {
  invoke('set_beatmap_hitsounds', { on: $('bm-hits').checked }).catch((e) => toast(`${e}`));
});
$('fps').addEventListener('change', () => {
  invoke('set_fps', { fps: Number($('fps').value) }).catch((e) => toast(`${e}`));
});
$('unload').addEventListener('click', () => invoke('unload').catch((e) => toast(`${e}`)));

$('autostart').addEventListener('change', async () => {
  try {
    await invoke('set_autostart', { on: $('autostart').checked });
    toast($('autostart').checked ? '已设置开机自启' : '已取消开机自启');
  } catch (e) {
    $('autostart').checked = !$('autostart').checked;
    toast(`设置失败：${e}`);
  }
});
$('log-rec').addEventListener('change', async () => {
  try {
    await invoke('set_log_enabled', { on: $('log-rec').checked });
    toast($('log-rec').checked ? '日志记录已开启' : '日志记录已关闭');
  } catch (e) {
    $('log-rec').checked = !$('log-rec').checked;
    toast(`设置失败：${e}`);
  }
});
$('log-export').addEventListener('click', async () => {
  try {
    const p = await invoke('export_log');
    if (p) toast(`日志已导出：${p}`);
  } catch (e) {
    toast(`${e}`);
  }
});
$('restart').addEventListener('click', async () => {
  toast('正在重启壁纸进程…');
  try {
    await invoke('restart_wallpaper');
  } catch (e) {
    toast(`重启失败：${e}`);
  }
});
$('quit').addEventListener('click', () => invoke('quit_app').catch(() => {}));

// ---- 数据目录手动选择(lazer / stable) ----
async function updateDirUI(id, manual) {
  const el = $(id);
  // 自动检测时也显示当前实际生效的目录(手动优先,改完即刷新)
  let effective = '';
  try {
    const st = await invoke('dir_status');
    const key = id.replace('-dir', '');
    effective = (key === 'lazer' ? st.lazerEffective : st.stableEffective) ?? '';
  } catch { /* 忽略 */ }
  el.textContent = manual ?? (effective ? `自动:${effective}` : '未检测到');
  el.title = manual ?? effective ?? '';
  $(id + '-reset').hidden = !manual;
}
async function pickAndSetDir(kind) {
  try {
    const path = await invoke('pick_data_dir', { kind });
    if (!path) return; // 用户取消
    await invoke('set_data_dir', { kind, path });
    updateDirUI(kind + '-dir', path);
    toast('已设置目录,曲库重新解析中…');
    loadLibrary(false);
  } catch (e) {
    toast(`设置失败：${e}`);
  }
}
$('lazer-dir-pick').addEventListener('click', () => pickAndSetDir('lazer'));
$('stable-dir-pick').addEventListener('click', () => pickAndSetDir('stable'));
async function resetDir(kind) {
  try {
    await invoke('set_data_dir', { kind, path: null });
    updateDirUI(kind + '-dir', null);
    toast('已恢复自动检测,曲库重新解析中…');
    loadLibrary(false);
  } catch (e) {
    toast(`${e}`);
  }
}
$('lazer-dir-reset').addEventListener('click', () => resetDir('lazer'));
$('stable-dir-reset').addEventListener('click', () => resetDir('stable'));

// ---- ffmpeg/ffprobe 手动路径(视频层 + 视频探测/BGM 时长;空 = 内置/PATH) ----
const ffmpegBins = { ffmpeg: null, ffprobe: null }; // 手动指定的完整路径
const pathBins = {}; // 实际生效路径 + 来源("manual"/"bundled"/"path"/"none")
async function refreshBinPaths() {
  try {
    const st = await invoke('ffmpeg_bin_status');
    pathBins.ffmpeg = st.ffmpeg;
    pathBins.ffprobe = st.ffprobe;
    pathBins.ffmpegSrc = st.ffmpegSrc;
    pathBins.ffprobeSrc = st.ffprobeSrc;
  } catch { /* 忽略 */ }
  updateBinsUI();
}
function updateBinsUI() {
  for (const kind of ['ffmpeg', 'ffprobe']) {
    const manual = ffmpegBins[kind];
    const el = $(kind + '-path');
    if (manual) {
      el.textContent = manual;
      el.title = manual;
    } else {
      const p = pathBins[kind];
      const src = pathBins[kind + 'Src'];
      el.textContent = !p ? 'PATH 未找到'
        : src === 'bundled' ? `内置:${p}`
          : `PATH:${p}`;
      el.title = p ?? '';
    }
    $(kind + '-reset').hidden = !manual;
  }
}
async function setBins(patch) {
  const ffmpeg = 'ffmpeg' in patch ? patch.ffmpeg : ffmpegBins.ffmpeg;
  const ffprobe = 'ffprobe' in patch ? patch.ffprobe : ffmpegBins.ffprobe;
  await invoke('set_ffmpeg_bins', { ffmpeg, ffprobe });
  ffmpegBins.ffmpeg = ffmpeg;
  ffmpegBins.ffprobe = ffprobe;
  updateBinsUI();
}
async function pickBin(kind) {
  try {
    const path = await window.__TAURI__.dialog.open({
      multiple: false,
      title: kind === 'ffmpeg' ? '选择 ffmpeg.exe' : '选择 ffprobe.exe',
      filters: [{ name: '可执行文件', extensions: ['exe'] }],
    });
    if (!path) return; // 用户取消
    await setBins({ [kind]: path });
    toast(`已设置 ${kind},下次播放生效`);
  } catch (e) {
    toast(`设置失败:${e}`);
  }
}
async function resetBin(kind) {
  try {
    await setBins({ [kind]: null });
    toast(`${kind} 已恢复 PATH 自动检测`);
  } catch (e) {
    toast(`设置失败:${e}`);
  }
}
$('ffmpeg-pick').addEventListener('click', () => pickBin('ffmpeg'));
$('ffprobe-pick').addEventListener('click', () => pickBin('ffprobe'));
$('ffmpeg-reset').addEventListener('click', () => resetBin('ffmpeg'));
$('ffprobe-reset').addEventListener('click', () => resetBin('ffprobe'));

// ---- 关闭窗口询问(未保存行为时弹窗;选择可记住,设置里可改) ----
listen('wall://close-ask', () => {
  $('close-modal').hidden = false;
}).catch(() => {});
function doClose(mode) {
  $('close-modal').hidden = true;
  invoke('close_window', { mode, remember: $('close-remember').checked }).catch((e) => toast(`${e}`));
}
$('close-to-tray').addEventListener('click', () => doClose('tray'));
$('close-exit').addEventListener('click', () => doClose('exit'));
$('close-cancel').addEventListener('click', () => {
  $('close-modal').hidden = true; // 留在原地,窗口不关
});
$('close-action').addEventListener('change', () => {
  const v = $('close-action').value;
  invoke('set_close_action', { action: v || null }).catch((e) => toast(`${e}`));
});

// ---- 设置分组展开状态记忆(localStorage,键为 summary 文本) ----
const GRP_KEY = 'settings-grp-open';
document.querySelectorAll('details.grp').forEach((d) => {
  const name = d.querySelector('summary')?.textContent?.trim() || '';
  d.addEventListener('toggle', () => {
    try {
      const saved = new Set(JSON.parse(localStorage.getItem(GRP_KEY) || '[]'));
      d.open ? saved.add(name) : saved.delete(name);
      localStorage.setItem(GRP_KEY, JSON.stringify([...saved]));
    } catch {}
  });
});
try {
  const saved = new Set(JSON.parse(localStorage.getItem(GRP_KEY) || '[]'));
  document.querySelectorAll('details.grp').forEach((d) => {
    const name = d.querySelector('summary')?.textContent?.trim() || '';
    if (saved.has(name)) d.open = true;
  });
} catch {}
