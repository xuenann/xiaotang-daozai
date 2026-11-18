//! Magpie 本地网关。渠道与账号都以网关返回为准，岛内不保存任何登录凭据。
use super::{Channel, Snapshot, Window};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const GATEWAY: &str = "http://127.0.0.1:3425";
const PATH: &str = "/v1/magpie/quotas";
pub(super) const OFFLINE: &str = "未连接 Magpie 本地网关（127.0.0.1:3425），请先启动 Magpie";
const CACHE_TTL: Duration = Duration::from_secs(90);

/// 网关返回的一条账号额度。渠道名、账号名、套餐都由 Magpie 决定。
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub channel: Channel,
    pub user: String,
    pub alias: String,
    pub plan: String,
    pub windows: Vec<Window>,
}
impl Entry {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot { plan: self.plan.clone(), windows: self.windows.clone(), at: chrono::Utc::now().timestamp(), ..Default::default() }
    }
}

fn cache() -> &'static Mutex<Option<(Instant, Vec<Entry>)>> {
    static CACHE: Mutex<Option<(Instant, Vec<Entry>)>> = Mutex::new(None);
    &CACHE
}

/// 只访问本机回环地址，因此不能跟随系统代理：环境变量里有 HTTP_PROXY 时也要直连网关。
fn local_agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(timeout).redirects(0).try_proxy_from_env(false).build()
}

/// 一次刷新周期内多个账号共用一份结果，避免每个账号各打一次网关。
pub(super) fn entries(force: bool) -> Result<Vec<Entry>, String> {
    let cached = cache().lock().map(|guard| guard.clone()).unwrap_or_default();
    if !force {
        if let Some((at, list)) = cached {
            if at.elapsed() < CACHE_TTL {
                return Ok(list);
            }
        }
    }
    let value = request()?;
    let list = parse(&value)?;
    if let Ok(mut guard) = cache().lock() {
        *guard = Some((Instant::now(), list.clone()));
    }
    Ok(list)
}

fn request() -> Result<Value, String> {
    let response = local_agent(Duration::from_secs(4)).get(&format!("{GATEWAY}{PATH}")).set("Accept", "application/json").call().map_err(|error| match error {
        ureq::Error::Status(404, _) => "Magpie 版本过旧，没有额度接口".to_string(),
        ureq::Error::Status(code, _) => format!("Magpie 额度接口暂不可用（{code}）"),
        _ => OFFLINE.to_string(),
    })?;
    let mut body = String::new();
    use std::io::Read;
    response.into_reader().take(4 * 1024 * 1024).read_to_string(&mut body).map_err(|_| "读取 Magpie 额度响应失败".to_string())?;
    serde_json::from_str(&body).map_err(|_| "Magpie 返回的不是有效 JSON".to_string())
}

fn parse(value: &Value) -> Result<Vec<Entry>, String> {
    let data = value["data"].as_array().ok_or_else(|| "Magpie 没有返回额度列表".to_string())?;
    let now = chrono::Utc::now().timestamp();
    let mut list = Vec::new();
    for item in data {
        let (Some(id), Some(user)) = (item["provider"].as_str().filter(|s| !s.trim().is_empty()), item["user"].as_str().filter(|s| !s.trim().is_empty())) else { continue };
        let windows: Vec<Window> = item["windows"].as_array().into_iter().flatten().filter_map(|window| one(window, now)).collect();
        // 没有额度周期的渠道（纯 API Key 之类）不占位置。
        if windows.is_empty() {
            continue;
        }
        let alias = item["alias"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or(user).trim().to_owned();
        list.push(Entry { channel: Channel { id: id.to_owned(), name: item["name"].as_str().unwrap_or(id).to_owned() }, user: user.to_owned(), alias, plan: item["plan"].as_str().unwrap_or_default().to_owned(), windows });
    }
    // 同一渠道里可能出现重名账号（例如同一手机号的多个 IAM 用户），加序号区分。
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    for entry in &mut list {
        let count = seen.entry((entry.channel.id.clone(), entry.alias.clone())).or_insert(0);
        *count += 1;
        if *count > 1 {
            entry.alias = format!("{} · {}", entry.alias, count);
        }
    }
    Ok(list)
}

fn one(window: &Value, now: i64) -> Option<Window> {
    let remaining = number(&window["remaining"]).or_else(|| number(&window["used"]).map(|used| 100.0 - used))?;
    let reset = super::reset(&window["resetsAt"]);
    Some(Window { label: label(window["name"].as_str().unwrap_or_default()), seconds: reset.map(|at| (at - now).max(0)), remaining: remaining.clamp(0.0, 100.0) as f32, reset, caption: amount(window) ,amount:None})
}

fn label(name: &str) -> String {
    match name {
        "Credits" => "积分".into(),
        "Extra credits" => "额外积分".into(),
        "7 days" => "7 天".into(),
        "30 days" => "30 天".into(),
        other => other.to_owned(),
    }
}

/// 剩余量 = 上限 − 已用；Magpie 的 display 是「已用 / 上限」，直接展示会读反。
fn amount(window: &Value) -> Option<String> {
    let limit = number(&window["limit"]).filter(|limit| *limit > 0.0)?;
    let used = number(&window["amount"]).or_else(|| number(&window["used"]).map(|percent| limit * percent / 100.0))?;
    Some(format!("{} / {}", trim(limit - used), trim(limit)))
}

fn trim(value: f64) -> String {
    if value.abs() >= 100.0 {
        return format!("{value:.0}");
    }
    let round = (value * 10.0).round() / 10.0;
    if round.fract() == 0.0 {
        format!("{round:.0}")
    } else {
        format!("{round:.1}")
    }
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok()).filter(|value| value.is_finite())
}

pub(super) fn scan() -> Result<Vec<Entry>, String> {
    entries(false)
}

fn pick<'a>(list: &'a [Entry], channel: &Channel, slot: &str) -> Option<&'a Entry> {
    list.iter().find(|entry| entry.channel.id == channel.id && entry.user == slot)
}

pub(super) fn fetch(account: &super::Account) -> Result<Snapshot, (String, u64)> {
    let channel = account.channel.as_ref().ok_or(("这个账号没有绑定 Magpie 渠道".to_string(), 0))?;
    let list = entries(false).map_err(|error| (error, 30))?;
    pick(&list, channel, account.slot.as_deref().unwrap_or_default()).map(Entry::snapshot).ok_or_else(|| ("Magpie 里已经没有这个账号，请在 Magpie 中重新登录".to_string(), 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> Value {
        json!({"object":"list","data":[
            {"provider":"workbuddy","name":"WorkBuddy","plan":"Free plan","user":"符号家","windows":[
                {"name":"Credits","used":0,"remaining":100,"resetsAt":"2030-11-30T15:59:59Z","display":"0 / 500","limit":500,"unit":"credits"},
                {"name":"Extra credits","used":1.8067,"remaining":98.1933,"display":"71.7 / 3969","amount":71.7,"limit":3969,"unit":"credits"}
            ]},
            {"provider":"codearts","name":"CodeArts","plan":"体验版","user":"iam::one","alias":"156****0024","windows":[
                {"name":"总积分","used":2.8,"remaining":97.2,"resetsAt":"2030-11-20T16:00:00Z","display":"213 / 7600","amount":212.94,"limit":7600,"unit":"credits"}
            ]},
            {"provider":"codearts","name":"CodeArts","plan":"体验版","user":"iam::two","alias":"156****0024","windows":[
                {"name":"总积分","used":0,"remaining":100,"display":"0 / 4500","limit":4500,"unit":"credits"}
            ]},
            {"provider":"api-only","name":"API Only","user":"key","windows":[]}
        ]})
    }

    #[test]
    fn entries_keep_channels_accounts_and_remaining_amounts_separate() {
        let list = parse(&payload()).unwrap();
        assert_eq!(list.len(), 3, "没有额度周期的渠道不应出现");
        assert_eq!(list[0].channel.id, "workbuddy");
        assert_eq!(list[0].alias, "符号家");
        assert_eq!(list[0].windows.len(), 2);
        assert_eq!(list[0].windows[0].label, "积分");
        assert_eq!(list[0].windows[0].caption.as_deref(), Some("500 / 500"));
        assert_eq!(list[0].windows[1].label, "额外积分");
        assert_eq!(list[0].windows[1].caption.as_deref(), Some("3897 / 3969"));
        assert!((list[0].windows[1].remaining - 98.1933).abs() < 0.01);
        assert_eq!(list[1].windows[0].caption.as_deref(), Some("7387 / 7600"));
        assert_eq!(list[1].alias, "156****0024");
        assert_eq!(list[2].alias, "156****0024 · 2", "同渠道重名账号要能区分");
        assert_eq!(list[2].user, "iam::two");
    }

    #[test]
    fn account_snapshot_matches_its_own_channel_and_user() {
        let list = parse(&payload()).unwrap();
        let channel = |id: &str| Channel { id: id.into(), name: id.into() };
        assert!(pick(&list, &channel("workbuddy"), "别人").is_none(), "不同账号不能拿到别人的额度");
        assert!(pick(&list, &channel("grok"), "符号家").is_none(), "同名账号跨渠道不能串");
        let entry = pick(&list, &channel("codearts"), "iam::two").unwrap();
        let snapshot = entry.snapshot();
        assert_eq!(snapshot.plan, "体验版");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].caption.as_deref(), Some("4500 / 4500"));
    }

    #[test]
    fn reset_and_amount_survive_missing_or_odd_fields() {
        let now = 1_700_000_000;
        let window = one(&json!({"name":"Credits","used":"12.5","limit":"1000","amount":"125"}), now).unwrap();
        assert_eq!(window.caption.as_deref(), Some("875 / 1000"));
        assert_eq!(window.reset, None);
        assert_eq!(window.remaining, 87.5);
        assert_eq!(one(&json!({"name":"Credits"}), now), None, "没有剩余量不能编造一个环");
        assert_eq!(one(&json!({"name":"Credits","remaining":0,"limit":0}), now).unwrap().caption, None);
    }
}
