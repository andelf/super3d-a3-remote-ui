//! 打印剩余时间估算。
//!
//! **协议里没有剩余时间**：`8/0` 状态帧固定 39 字节，从 `printStatus` 到
//! `endStop` 全部用满，没有留时间字段；官方应用（Super3D 的 starfish-net
//! 面板）对这类设备也只画一个百分比圆环，不显示时间。厂商自己的 `剩余时间`
//! 是 Luban 里 HTTP(8080) 机型和串口 sender 才有的，这台机器没有 Web 服务。
//!
//! 只能自己估，两个来源，**优先级不同**：
//! 1. **切片器总时长**（优先）：G-code 头部 `;TIME:` / `; estimated printing time`，
//!    剩余 = 总时长 × (100 - p)/100。只要文件是我们自己传上去的，这个值就有；
//!    它是切片器排完路径算出来的真实打印时长，比任何现场外推都准，所以**有它就不用别的**。
//! 2. **现场测速**（兜底）：机内老文件不是本机传的，头部拿不回来（协议没有下载方向），
//!    只能把「进度到位时刻」采样做最小二乘，斜率就是每 1% 要几秒，
//!    剩余 = (100 - p) × 斜率。误差比总时长那套大，界面上标「实测」以示区别。

/// 只保留最近 15 分钟的采样。
const WINDOW_S: f64 = 900.0;
/// 跨度小于这个值时测速值不可信（0.1% 一格，几格误差就很大）。
const MIN_SPAN_S: f64 = 120.0;
/// 窗口内进度至少涨这么多才算「动过」。
const MIN_DPCT: f32 = 0.5;
/// 估出来超过一周直接丢掉（多半是进度卡住不动算出来的垃圾）。
const MAX_REMAIN_S: f64 = 7.0 * 86400.0;

/// 剩余时间估算器。喂进度、问剩余，别的什么都不管。
#[derive(Debug, Default)]
pub struct Eta {
    /// 切片器给的**总**时长（秒）。不知道就是 None。
    total_s: Option<f64>,
    /// `(秒, 百分比)` 采样，按时间递增。
    samples: Vec<(f64, f32)>,
    /// 上一帧的进度，用来判断「没动」和「倒退了」。
    last_pct: Option<f32>,
}

impl Eta {
    /// 记下这一次打印的总时长（来自 G-code 头部）。
    pub fn set_total(&mut self, secs: f64) {
        self.total_s = if secs.is_finite() && secs > 0.0 {
            Some(secs)
        } else {
            None
        };
    }

    /// 不知道总时长（打印的不是我们上传的文件）。
    pub fn clear_total(&mut self) {
        self.total_s = None;
    }

    pub fn total(&self) -> Option<f64> {
        self.total_s
    }

    /// 当前这个数字是不是来自切片器总时长（`false` = 现场测速外推的，更糙）。
    pub fn from_total(&self) -> bool {
        self.total_s.is_some()
    }

    /// 新的一次打印：丢掉采样，总时长保留（同一个文件重打仍然有效）。
    pub fn reset_progress(&mut self) {
        self.samples.clear();
        self.last_pct = None;
    }

    /// 彻底归零（断开连接 / 打印结束）。
    pub fn reset(&mut self) {
        self.reset_progress();
        self.total_s = None;
    }

    /// 喂一帧进度。`t` 是秒（起点随意，只要单调），`pct` 是 0..100。
    ///
    /// 只在进度**变化**时留样本：设备是 0.1% 一格，等于「每到一个整格记一次时刻」，
    /// 这正好是测速要的东西；进度不动时记时间只会把斜率拉平。
    pub fn observe(&mut self, t: f64, pct: f32) {
        if !t.is_finite() || !pct.is_finite() {
            return;
        }
        let pct = pct.clamp(0.0, 100.0);
        if let Some(prev) = self.last_pct {
            if pct + 2.0 < prev {
                // 进度倒退超过 2%：换了文件或者重新开始
                self.reset_progress();
            } else if (pct - prev).abs() < 0.05 {
                return;
            }
        }
        self.samples.push((t, pct));
        self.last_pct = Some(pct);
        let cut = t - WINDOW_S;
        while self.samples.len() > 2 && self.samples[0].0 < cut {
            self.samples.remove(0);
        }
    }

    /// 当前采样窗口的时间跨度（秒）。
    pub fn span(&self) -> f64 {
        match (self.samples.first(), self.samples.last()) {
            (Some(a), Some(b)) => (b.0 - a.0).max(0.0),
            _ => 0.0,
        }
    }

    /// 每 1% 需要多少秒（最小二乘斜率取倒数）。样本不够就 None。
    fn secs_per_pct(&self) -> Option<f64> {
        let n = self.samples.len();
        if n < 3 {
            return None;
        }
        let span = self.span();
        let dpct = self.samples[n - 1].1 - self.samples[0].1;
        if span < MIN_SPAN_S || dpct < MIN_DPCT {
            return None;
        }
        let nf = n as f64;
        let mt = self.samples.iter().map(|s| s.0).sum::<f64>() / nf;
        let mp = self.samples.iter().map(|s| s.1 as f64).sum::<f64>() / nf;
        let (mut num, mut den) = (0.0f64, 0.0f64);
        for (t, p) in &self.samples {
            num += (t - mt) * (*p as f64 - mp);
            den += (t - mt) * (t - mt);
        }
        if den <= 0.0 || num <= 0.0 {
            return None;
        }
        let per_pct = den / num; // s / pct
        if per_pct.is_finite() && per_pct > 0.0 {
            Some(per_pct)
        } else {
            None
        }
    }

    /// 剩余秒数。两个来源都没有就 None（UI 该显示「估算中」而不是瞎猜）。
    ///
    /// 有切片器总时长时**只**用它：那是切片器算出来的真实值，测速只是拿过去的
    /// 平均速度往后外推，一旦换层/换填充比例就飘，没理由拿它去污染准确值。
    pub fn remaining(&self, pct: f32) -> Option<f64> {
        let pct = pct.clamp(0.0, 100.0) as f64;
        if pct >= 99.95 {
            return Some(0.0);
        }
        let left = 100.0 - pct;
        if let Some(t) = self.total_s {
            return Some((t * left / 100.0).max(0.0));
        }
        self.secs_per_pct()
            .map(|s| s * left)
            .filter(|s| *s >= 0.0 && *s < MAX_REMAIN_S)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_slicer_total() {
        let mut e = Eta::default();
        e.set_total(7200.0);
        // 没有采样，只能按百分比平摊
        assert_eq!(e.remaining(25.0), Some(5400.0));
        assert_eq!(e.remaining(100.0), Some(0.0));
    }

    #[test]
    fn learns_the_real_pace() {
        // 每 60 秒涨 1%，t=0..600 → 10%
        let mut e = Eta::default();
        let mut t = 0.0;
        while t <= 600.0 {
            e.observe(t, (t / 60.0) as f32);
            t += 10.0;
        }
        let r = e.remaining(10.0).unwrap();
        assert!((r - 5400.0).abs() < 60.0, "剩余 {r}，应约 5400");
    }

    #[test]
    fn static_progress_has_no_estimate() {
        let mut e = Eta::default();
        for i in 0..100 {
            e.observe(i as f64 * 5.0, 42.0);
        }
        assert!(e.remaining(42.0).is_none());
        assert_eq!(e.samples.len(), 1);
    }

    #[test]
    fn restart_clears_samples() {
        let mut e = Eta::default();
        let mut t = 0.0;
        while t <= 600.0 {
            e.observe(t, (t / 60.0) as f32);
            t += 10.0;
        }
        assert!(e.remaining(10.0).is_some());
        // 重新开始打同一份文件：进度回到 0
        e.observe(700.0, 0.0);
        assert!(e.samples.len() <= 2, "采样没清干净：{:?}", e.samples);
        assert!(e.remaining(0.0).is_none());
    }

    #[test]
    fn slicer_total_wins_over_measured_pace() {
        // 头部写 2h（72s/1%），现场跑出 60s/1%：有总时长就一律按总时长算，
        // 哪怕样本已经攒了 15 分钟——切片器给的才是真实值。
        let mut e = Eta::default();
        e.set_total(7200.0);
        let mut t = 0.0;
        while t <= 900.0 {
            e.observe(t, (t / 60.0) as f32);
            t += 10.0;
        }
        let r = e.remaining(15.0).unwrap(); // 85×72 = 6120，不能飘到 5100
        assert!((r - 6120.0).abs() < 1.0, "剩余 {r}，应严格按总时长 6120");
        assert!(e.from_total());
    }

    #[test]
    fn without_total_the_pace_is_used() {
        // 机内老文件：没有总时长，只能按实测速度外推（85 格 × 60s）
        let mut e = Eta::default();
        assert!(!e.from_total());
        let mut t = 0.0;
        while t <= 600.0 {
            e.observe(t, (t / 60.0) as f32);
            t += 10.0;
        }
        let r = e.remaining(15.0).unwrap();
        assert!((r - 5100.0).abs() < 120.0, "剩余 {r}，应约 5100");
    }

    #[test]
    fn short_window_still_prefers_total() {
        let mut e = Eta::default();
        e.set_total(3600.0);
        let mut t = 0.0;
        while t <= 60.0 {
            e.observe(t, (t / 60.0) as f32);
            t += 5.0;
        }
        // 跨度只有 60s < MIN_SPAN_S，测速值不参与
        let r = e.remaining(1.0).unwrap();
        assert!((r - 3600.0 * 0.99).abs() < 1.0, "剩余 {r}");
    }
}
