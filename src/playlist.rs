//! 播放列表:单曲循环 / 顺序 / 随机。曲目只来自用户显式添加(单曲 /
//! 批量),任何曲库操作都不自动填充;曲目结束(子进程 Ended 事件)时由
//! 父进程 advance 并加载下一首。

use crate::lazer::{LazerBeatmap, LazerLibrary, LazerSet};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlayMode {
    /// 单曲循环(子进程内循环,不切歌)。
    SingleLoop,
    /// 顺序播放:收藏夹按收藏顺序,否则全部谱面按曲库顺序(默认)。
    #[default]
    ListOrder,
    /// 随机播放(同一来源列表内随机,不与上一首重复)。
    Random,
}

impl PlayMode {
    pub fn parse(s: &str) -> Option<PlayMode> {
        match s {
            "single_loop" => Some(PlayMode::SingleLoop),
            "list_order" => Some(PlayMode::ListOrder),
            "random" => Some(PlayMode::Random),
            _ => None,
        }
    }
}

/// 一首要播的曲目:谱面集 id + 难度标识 + 队列内唯一 id(同一曲目重复
/// 入列也能区分删除)。难度为 `None` = **谱面集级条目**(默认入列单位):
/// 播放时才按目标星级解析难度,播放中可随时切换;`Some(sha2)` 是用户
/// 显式指定的难度(lazer = blob sha2,stable = .osu 文件名)。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub set_id: String,
    pub sha2: Option<String>,
    pub qid: u64,
    /// osu! legacy mod 位(HD/HR/EZ/DT/HT/NC;加入播放列表时选定)。
    pub mods: u32,
    /// 显示快照(入列/播放时写入):播放列表渲染不依赖曲库解析 ——
    /// 启动秒显,曲库就绪后刷新精化(封面/难度星级)。
    pub title: String,
    pub artist: String,
    pub length_ms: f64,
}

impl Track {
    /// 同一首曲目(集合 + 难度),忽略 qid。任一侧是谱面集级(None)即视为
    /// 同一首 —— 集合级条目可以和它任何一个难度对齐/去重。
    fn same_song(&self, other: &Track) -> bool {
        self.set_id == other.set_id
            && match (&self.sha2, &other.sha2) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

#[derive(Debug, Clone)]
pub struct Playlist {
    pub mode: PlayMode,
    tracks: Vec<Track>,
    /// 播放顺序:tracks 下标的排列。顺序 = 0..n;随机 = 一次性洗好的
    /// 排列(整个排列播完前不重复)。
    order: Vec<usize>,
    /// 当前曲目在 order 中的位置。
    pos: usize,
    /// 随机数发生器状态(LCG;壁纸场景不需要密码学质量)。
    rng: u64,
    /// qid 发号器。
    next_qid: u64,
}

impl Default for Playlist {
    fn default() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15)
            ^ (std::process::id() as u64).rotate_left(32);
        Playlist {
            mode: PlayMode::ListOrder,
            tracks: Vec::new(),
            order: Vec::new(),
            pos: 0,
            rng: seed | 1,
            next_qid: 1,
        }
    }
}

impl Playlist {
    fn next_rand(&mut self) -> u64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Fisher-Yates 洗出 0..n 的排列;avoid 首位与给定曲目重复
    /// (轮末重洗时不接着重播上轮最后一首)。
    fn shuffled(&mut self, n: usize, avoid_first: Option<&Track>) -> Vec<usize> {
        let mut order: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            let j = (self.next_rand() as usize) % (i + 1);
            order.swap(i, j);
        }
        if n > 1 {
            if let (Some(t), Some(first)) = (avoid_first, order.first()) {
                if self.tracks[*first].same_song(t) {
                    order.swap(0, 1);
                }
            }
        }
        order
    }

    /// 切换播放模式(曲目保持不变,不自动从曲库补内容):随机模式对当前
    /// 曲目列表重洗一次,顺序模式回到入列顺序;播放位置始终指向当前曲目。
    pub fn set_mode(&mut self, mode: PlayMode) {
        let current = self.current();
        self.mode = mode;
        let n = self.tracks.len();
        self.order = if mode == PlayMode::Random {
            self.shuffled(n, None)
        } else {
            (0..n).collect()
        };
        self.pos = match &current {
            Some(t) => self
                .order
                .iter()
                .position(|&o| self.tracks[o].same_song(t))
                .unwrap_or(0),
            None => 0,
        };
    }

    /// 追加入列(默认**按谱面集去重**:同一谱面集的任何条目已存在则跳过,
    /// 返回 false)。`sha2 = None` = 谱面集级条目(播放时按目标星级解析
    /// 难度);难度 chip 传入 sha2 则固定难度。title/artist/length_ms 为
    /// 显示快照,宿主从曲库取到后传入。
    /// 追加入列;**同谱面集已存在 = 原位替换参数**(难度锁定/mods/显示
    /// 快照),位置与 qid 不变。返回是否为新插入(false = 替换了已有)。
    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &mut self,
        set_id: String,
        sha2: Option<String>,
        mods: u32,
        title: String,
        artist: String,
        length_ms: f64,
    ) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.set_id == set_id) {
            t.sha2 = sha2;
            t.mods = mods;
            t.title = title;
            t.artist = artist;
            t.length_ms = length_ms;
            return false;
        }
        let qid = self.next_qid;
        self.next_qid += 1;
        let t = Track { set_id, sha2, mods, qid, title, artist, length_ms };
        self.order.push(self.tracks.len());
        self.tracks.push(t);
        true
    }

    /// 编辑队列项(右键菜单):覆盖 mods 与难度锁定方式
    /// (`sha2 = Some` = 锁定谱面,`None` = 谱面集级按星级解析)。
    /// 返回编辑后的条目(不存在 = None)。
    pub fn set_entry(&mut self, qid: u64, mods: u32, sha2: Option<String>) -> Option<Track> {
        let t = self.tracks.iter_mut().find(|t| t.qid == qid)?;
        t.mods = mods;
        t.sha2 = sha2;
        Some(t.clone())
    }

    /// 批量设置 mods(多选右键"修改 mods"):难度锁定不动(跨谱面集
    /// 难度不可统一)。返回被修改条目中的当前曲目(供原位重载)。
    pub fn set_mods_batch(&mut self, qids: &[u64], mods: u32) -> Option<Track> {
        let cur_qid = self.current().map(|t| t.qid);
        let mut current_hit = None;
        for qid in qids {
            if let Some(t) = self.tracks.iter_mut().find(|t| t.qid == *qid) {
                t.mods = mods;
                if cur_qid == Some(*qid) {
                    current_hit = Some(t.clone());
                }
            }
        }
        current_hit
    }

    /// 移除队列项(按 qid)。当前曲被移除时播放位置自然落到下一首。
    pub fn remove(&mut self, qid: u64) {
        let Some(idx) = self.tracks.iter().position(|t| t.qid == qid) else { return };
        self.tracks.remove(idx);
        // order 里的下标整体前移,并保住当前曲目位置
        let removed_order_pos = self.order.iter().position(|&o| o == idx);
        self.order.retain(|&o| o != idx);
        for o in &mut self.order {
            if *o > idx {
                *o -= 1;
            }
        }
        if let Some(rp) = removed_order_pos {
            if rp < self.pos {
                self.pos -= 1;
            }
        }
        if self.pos >= self.order.len() {
            self.pos = self.order.len().saturating_sub(1);
        }
    }

    /// 清空播放列表(保留模式)。
    pub fn clear(&mut self) {
        self.tracks.clear();
        self.order.clear();
        self.pos = 0;
    }

    /// 恢复上次会话的播放列表(启动时)。曲目按保存顺序重入(含显示
    /// 快照),当前曲目由 pos 指定;不再存在的谱面由 prune_missing 清理。
    pub fn restore(&mut self, saved: &[crate::settings::SavedTrack], pos: usize, mode: PlayMode) {
        self.mode = mode;
        self.tracks = saved
            .iter()
            .map(|s| {
                let qid = self.next_qid;
                self.next_qid += 1;
                Track {
                    set_id: s.set_id.clone(),
                    sha2: s.sha2.clone(),
                    mods: s.mods,
                    qid,
                    title: s.title.clone(),
                    artist: s.artist.clone(),
                    length_ms: s.length_ms,
                }
            })
            .collect();
        let n = self.tracks.len();
        self.order = (0..n).collect();
        self.pos = if n == 0 { 0 } else { pos.min(n - 1) };
    }

    /// 持久化快照:(曲目列表按入列顺序含显示元数据, 当前 order 位置)。
    pub fn snapshot(&self) -> (Vec<Track>, usize) {
        (self.tracks.clone(), self.pos)
    }

    /// 就地刷新条目显示快照(按同曲目匹配,通常命中当前条目):播放时
    /// 解析出的标题/艺术家/难度时长回写,秒显信息保持新鲜。
    pub fn backfill_meta(&mut self, probe: &Track, title: String, artist: String, length_ms: f64) {
        for t in &mut self.tracks {
            if t.same_song(probe) {
                t.title = title.clone();
                t.artist = artist.clone();
                t.length_ms = length_ms;
            }
        }
    }

    /// 清理当前曲库中已不存在的曲目(切源恢复后调用:持久化列表可能
    /// 混着另一个源的曲目 id,点播必报"谱面集不在曲库中")。
    pub fn prune_missing(&mut self, lib: &LazerLibrary) {
        let valid: std::collections::HashSet<&str> =
            lib.sets.iter().map(|s| s.id.as_str()).collect();
        let old_pos_track = self.tracks.get(*self.order.get(self.pos).unwrap_or(&0)).cloned();
        let tracks = std::mem::take(&mut self.tracks);
        self.tracks = tracks
            .into_iter()
            .filter(|t| valid.contains(t.set_id.as_str()))
            .collect();
        self.order = (0..self.tracks.len()).collect();
        self.pos = match old_pos_track {
            Some(t) if valid.contains(t.set_id.as_str()) => {
                self.tracks.iter().position(|x| x.same_song(&t)).unwrap_or(0)
            }
            _ => 0,
        };
    }

    /// UI 点播:曲目在列表中则把播放位置对齐过去;不在列表 = 临时播放,
    /// **不自动入列**(要入列走显式的 add / 批量添加)。`sha2 = None`
    /// (双击谱面集行)可与该集任何难度条目对齐。返回是否对齐成功。
    pub fn align_current(&mut self, set_id: String, sha2: Option<String>) -> bool {
        let probe = Track { set_id, sha2, qid: 0, ..Default::default() };
        match self.tracks.iter().position(|t| t.same_song(&probe)) {
            Some(i) => {
                self.pos = self.order.iter().position(|&o| o == i).unwrap_or(0);
                true
            }
            None => false,
        }
    }

    /// 当前曲目。
    pub fn current(&self) -> Option<Track> {
        self.tracks.get(*self.order.get(self.pos)?).cloned()
    }

    /// 队列 UI 视角 = 实际播放顺序(随机 = 洗好的顺序,所见即所播);
    /// 顺序模式恒为入列顺序,并支持手动拖动排序。
    pub fn queue(&self) -> Vec<Track> {
        self.order.iter().map(|&i| self.tracks[i].clone()).collect()
    }

    /// 手动排序(UI 顺序模式拖动):第 from 项移动到第 to 位(入列顺序
    /// 下标)。tracks 重排,order 按曲目 qid 原样重映射 —— 随机模式的
    /// 洗牌相对序也保持;播放头对齐当前曲目。
    pub fn reorder(&mut self, from: usize, to: usize) {
        if from == to || from >= self.tracks.len() || to >= self.tracks.len() {
            return;
        }
        let current = self.current();
        let seq: Vec<u64> = self.order.iter().map(|&i| self.tracks[i].qid).collect();
        let t = self.tracks.remove(from);
        self.tracks.insert(to, t);
        self.order = seq
            .iter()
            .map(|qid| self.tracks.iter().position(|x| x.qid == *qid).expect("order/tracks 失配"))
            .collect();
        self.pos = match &current {
            Some(t) => self
                .order
                .iter()
                .position(|&o| self.tracks[o].same_song(t))
                .unwrap_or(0),
            None => 0,
        };
    }

    /// 下一首;单曲循环返回 None(子进程自己循环)。顺序/随机沿 order
    /// 前进;整轮播完后随机模式重新洗牌(不与上轮末尾重复),顺序模式
    /// 回到开头。
    pub fn advance(&mut self) -> Option<Track> {
        if self.order.is_empty() {
            return None;
        }
        match self.mode {
            PlayMode::SingleLoop => return None,
            _ => {}
        }
        self.pos += 1;
        if self.pos >= self.order.len() {
            self.pos = 0;
            if self.mode == PlayMode::Random {
                let last = self.tracks[*self.order.last().unwrap()].clone();
                self.order = self.shuffled(self.tracks.len(), Some(&last));
            }
        }
        self.tracks.get(self.order[self.pos]).cloned()
    }

    /// 手动"下一首"(托盘/菜单):单曲循环模式下也沿 order 前进,
    /// 其余与 advance 一致。
    pub fn advance_manual(&mut self) -> Option<Track> {
        if self.order.is_empty() {
            return None;
        }
        self.pos += 1;
        if self.pos >= self.order.len() {
            self.pos = 0;
            if self.mode == PlayMode::Random {
                let last = self.tracks[*self.order.last().unwrap()].clone();
                self.order = self.shuffled(self.tracks.len(), Some(&last));
            }
        }
        self.tracks.get(self.order[self.pos]).cloned()
    }

    /// 上一首(随机模式下也按 order 回退,便于回听)。
    pub fn rewind(&mut self) -> Option<Track> {
        if self.order.is_empty() {
            return None;
        }
        self.pos = if self.pos == 0 { self.order.len() - 1 } else { self.pos - 1 };
        self.tracks.get(self.order[self.pos]).cloned()
    }
}

/// 谱面集难度挑选:target = None 播**最难**(最高星);Some(x) 播星级
/// 最接近 x 的难度。曲库已只保留 std 难度。
/// 点播难度挑选策略:Hardest(lazer 默认)/ Easiest / Custom(星级
/// 绝对值最接近)。持久化为 target_star:None=Hardest,Some(-1)=Easiest,
/// Some(x>0)=Custom≈x。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PickPolicy {
    Hardest,
    Easiest,
    Custom(f64),
}

/// 持久化 target_star(Option<f64>) → 挑选策略:None=最难,
/// Some(-1)=最简单,Some(x>0)=自定义星级 x。
pub fn policy_of(target: Option<f64>) -> PickPolicy {
    match target {
        None => PickPolicy::Hardest,
        Some(t) if t < 0.0 => PickPolicy::Easiest,
        Some(t) => PickPolicy::Custom(t),
    }
}

pub fn pick_diff<'a>(set: &'a LazerSet, policy: PickPolicy) -> &'a LazerBeatmap {
    let mut best = &set.beatmaps[0];
    match policy {
        PickPolicy::Hardest => {
            for b in &set.beatmaps {
                if b.star_rating > best.star_rating {
                    best = b;
                }
            }
        }
        PickPolicy::Easiest => {
            for b in &set.beatmaps {
                if b.star_rating < best.star_rating {
                    best = b;
                }
            }
        }
        PickPolicy::Custom(t) => {
            for b in &set.beatmaps {
                if (b.star_rating - t).abs() < (best.star_rating - t).abs() {
                    best = b;
                }
            }
        }
    }
    best
}

/// [`pick_diff`] 的离线版:在路径缓存的难度表上按同一策略挑选,
/// 播放列表快路径(不解析数据库)用。
pub fn pick_cached_diff<'a>(
    diffs: &'a [crate::pathcache::CachedDiff],
    policy: PickPolicy,
) -> Option<&'a crate::pathcache::CachedDiff> {
    let mut it = diffs.iter();
    let mut best = it.next()?;
    for b in it {
        let better = match policy {
            PickPolicy::Hardest => b.star > best.star,
            PickPolicy::Easiest => b.star < best.star,
            PickPolicy::Custom(t) => (b.star - t).abs() < (best.star - t).abs(),
        };
        if better {
            best = b;
        }
    }
    Some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_remove_clear_current() {
        let mut p = Playlist::default();
        // 去重:同一谱面集重复入列被跳过(默认行为)
        let mut p = Playlist::default();
        p.add("a".into(), Some("x".into()), 0, String::new(), String::new(), 0.0);
        // 重复入列 = 原位替换参数(位置/qid 不变),不是跳过
        assert!(!p.add("a".into(), None, 0, String::new(), String::new(), 0.0), "重复入列=替换(false)");
        let q0 = p.queue()[0].qid;
        assert!(!p.add("a".into(), Some("y".into()), 8, String::new(), String::new(), 0.0), "再次替换");
        assert_eq!(p.queue().len(), 1, "条目数不变");
        assert_eq!(p.queue()[0].qid, q0, "qid/位置不变");
        assert_eq!(p.queue()[0].sha2.as_deref(), Some("y"), "难度锁定被替换");
        assert_eq!(p.queue()[0].mods, 8, "mods 被替换");
        p.add("b".into(), Some("y".into()), 0, String::new(), String::new(), 0.0);
        p.add("c".into(), None, 0, String::new(), String::new(), 0.0); // 谱面集级条目(默认单位)
        assert_eq!(p.queue().len(), 3);
        // 移除中间一首
        let qid_b = p.queue().iter().find(|t| t.set_id == "b").unwrap().qid;
        p.remove(qid_b);
        assert_eq!(p.queue().len(), 2);
        // 当前曲被移除后,播放位置落到下一首
        // a 的锁定难度已被上面的替换改为 "y"
        assert!(p.align_current("a".into(), Some("y".into())));
        assert_eq!(p.current().unwrap().set_id, "a");
        let qid_a = p.current().unwrap().qid;
        p.remove(qid_a);
        assert_eq!(p.current().unwrap().set_id, "c");
        // 点播列表中的曲目 → 对齐;不在列表 → 不入列(false)
        assert!(p.align_current("c".into(), Some("z".into())), "集合级条目与具体难度对齐");
        assert!(!p.align_current("d".into(), Some("w".into())), "临时播放不自动入列");
        assert_eq!(p.queue().len(), 1, "未对齐时列表不变");
        assert_eq!(p.current().unwrap().set_id, "c", "未对齐时当前曲不变");
        // 模式切换:不增删曲目,随机模式整轮不重复
        p.set_mode(PlayMode::Random);
        assert_eq!(p.queue().len(), 1);
        p.set_mode(PlayMode::ListOrder);
        // 清空
        p.clear();
        assert!(p.current().is_none());
        // 洗牌:整轮不重复
        let mut r = Playlist::default();
        for i in 0..50 { r.add(format!("s{i}"), Some(format!("h{i}")), 0, String::new(), String::new(), 0.0); }
        r.rebuild_shuffle_for_test();
    }

    impl Playlist {
        fn rebuild_shuffle_for_test(&mut self) {
            self.mode = PlayMode::Random;
            self.order = self.shuffled(self.tracks.len(), None);
            let mut seen = std::collections::HashSet::new();
            for &o in &self.order {
                assert!(seen.insert(o), "洗牌出现重复");
            }
        }
    }
}
