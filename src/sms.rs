//! 短信接收（v0.6.51）。
//!
//! PC 侧没有读取手机短信的系统接口，所以由手机端转发过来：
//! Android 上装一个转发 App（SmsForwarder 之类），把收到的短信 `POST` 到本机局域网端口。
//! 这里只做三件事：接住请求、认出字段、交给 [`crate::otp`] 提验证码。
//!
//! 不引 HTTP 框架：一个 `TcpListener` 加手写 HTTP/1.1 够用，也不给安装包增重。
//! 原文只在内存里流转，不落盘。

use crate::otp;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 3426;
/// 转发地址的路径部分：`http://<本机局域网 IP>:<端口>/sms`。
pub const PATH: &str = "/sms";
const HEAD_LIMIT: usize = 8 * 1024;
const BODY_LIMIT: usize = 32 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(4);
/// 界面最多保留的条数；验证码是一次性的，不需要长期堆着。
pub const KEEP: usize = 80;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: u64,
    pub from: String,
    pub text: String,
    /// Unix 秒。手机给的时间戳可能缺失或格式各异，认不出来就取服务器当前时间。
    pub at: i64,
    pub code: Option<String>,
    pub score: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub running: bool,
    pub port: u16,
    pub error: Option<String>,
}

impl Default for Status {
    fn default() -> Self {
        Self { running: false, port: DEFAULT_PORT, error: None }
    }
}

/// 转发端要带的令牌。用系统随机种子（`RandomState`）混时间生成，
/// 不引 `rand`：这个令牌只用于挡住同一局域网里的误发，不是密码。
pub fn new_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(24);
    for round in 0u64..3 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(round);
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        );
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out.truncate(24);
    out
}

/// 本机在局域网里的 IPv4；拿不到就返回 `None`（设置页会给出提示）。
pub fn local_ipv4() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // UDP 的 connect 只查路由表，不会真的发包。
    socket.connect("8.8.8.8:80").ok()?;
    let address = socket.local_addr().ok()?.ip();
    if address.is_loopback() || address.is_unspecified() { None } else { Some(address.to_string()) }
}

// ---------------------------------------------------------------- 接收线程

/// 后台接收器。`sync` 按当前设置重启监听，`try_recv` 取已解析的短信。
pub struct Inbox {
    rx: Option<Receiver<Message>>,
    stop: Option<Arc<AtomicBool>>,
    running: bool,
    port: u16,
    token: String,
    error: Option<String>,
}

impl Default for Inbox {
    fn default() -> Self {
        Self { rx: None, stop: None, running: false, port: DEFAULT_PORT, token: String::new(), error: None }
    }
}

impl Inbox {
    /// 开关、端口或令牌变化时才会真正重启监听；否则什么都不做。
    pub fn sync(&mut self, enabled: bool, port: u16, token: String) {
        let unchanged = self.running && self.port == port && self.token == token;
        if enabled && unchanged {
            return;
        }
        self.shutdown();
        self.port = port;
        self.token = token.clone();
        if !enabled {
            self.error = None;
            return;
        }
        match listen(port, token) {
            Ok((rx, stop)) => {
                self.rx = Some(rx);
                self.stop = Some(stop);
                self.running = true;
                self.error = None;
            }
            Err(error) => {
                self.running = false;
                self.error = Some(error);
            }
        }
    }

    pub fn try_recv(&mut self) -> Option<Message> {
        self.rx.as_ref()?.try_recv().ok()
    }

    pub fn status(&self) -> Status {
        Status { running: self.running, port: self.port, error: self.error.clone() }
    }

    fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.rx = None;
        self.running = false;
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn listen(port: u16, token: String) -> Result<(Receiver<Message>, Arc<AtomicBool>), String> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .map_err(|error| format!("端口 {port} 无法监听：{error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("端口 {port} 设置非阻塞失败：{error}"))?;
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("daozai-sms".into())
        .spawn(move || {
            let mut next_id = 0u64;
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => serve(&mut stream, &token, &tx, &mut next_id),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(120));
                    }
                    // 连接在 accept 阶段被重置很常见（手机重试），退让一下就继续。
                    Err(_) => std::thread::sleep(Duration::from_millis(250)),
                }
            }
        })
        .map_err(|error| format!("短信接收线程启动失败：{error}"))?;
    Ok((rx, stop))
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn serve(stream: &mut TcpStream, token: &str, tx: &Sender<Message>, next_id: &mut u64) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let request = match read_http(stream) {
        Ok(request) => request,
        Err(code) => {
            respond(stream, code, &format!("{{\"ok\":false,\"error\":\"{}\"}}", reason(code)));
            return;
        }
    };
    let fields = fields(&request.body, &request.query);
    if request.method != "POST" && request.method != "GET" {
        respond(stream, 405, "{\"ok\":false,\"error\":\"只接受 POST 或 GET\"}");
        return;
    }
    // 根路径给一句人话，方便用手机浏览器确认「网络通不通、防火墙放没放」。
    if request.path == "/" || request.path.is_empty() {
        respond(stream, 200, "岛仔短信接收正常，请用 POST /sms 转发");
        return;
    }
    if request.path != PATH {
        respond(stream, 404, "{\"ok\":false,\"error\":\"未知路径\"}");
        return;
    }
    if !authorized(&fields, &request.headers, token) {
        respond(stream, 401, "{\"ok\":false,\"error\":\"token 不正确\"}");
        return;
    }
    let text = pick(&fields, &["content", "message", "body", "text", "sms", "msg", "desc"])
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        respond(stream, 400, "{\"ok\":false,\"error\":\"缺少短信内容\"}");
        return;
    }
    // 模板里的占位符没被 App 替换时，正文就是这个字面量（旧写法 [sms]、新版写法 {{SMS}}）。
    // 直接拒掉并说明原因，否则列表里会多出一条内容叫「[sms]」的假短信。
    if is_placeholder(&text) {
        respond(stream, 400, "{\"ok\":false,\"error\":\"模板占位符未替换：SmsForwarder 3.x 用 {{SMS}}，2.x 用 [sms]；并确认规则里没开「自定义模板」\"}");
        return;
    }
    let from = pick(&fields, &["from", "sender", "phone", "number", "address", "mobile", "sendernumber"])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("未知号码")
        .to_string();
    let at = when(pick(&fields, &["timestamp", "time", "receive_time", "receivetime", "date", "datetime"]));
    let code = otp::extract("", &text, &[]);
    *next_id += 1;
    let message = Message {
        id: *next_id,
        from,
        text,
        at,
        code: code.as_ref().map(|found| found.text.clone()),
        score: code.map(|found| found.score).unwrap_or(0),
    };
    // 接收端已经关掉时 send 会失败，这时不需要额外动作。
    let _ = tx.send(message.clone());
    let body = match &message.code {
        Some(code) => format!("{{\"ok\":true,\"code\":\"{code}\"}}"),
        None => "{\"ok\":true,\"code\":null}".into(),
    };
    respond(stream, 200, &body);
}

fn read_http(stream: &mut TcpStream) -> Result<Request, u16> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > HEAD_LIMIT {
            return Err(431);
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(400),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(_) => return Err(400),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_ascii_uppercase();
    let target = first.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let declared = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    if declared > BODY_LIMIT {
        return Err(413);
    }
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < declared {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
            Err(_) => break,
        }
    }
    // 没有 Content-Length 时保留已读到的部分，宽容一点；有则按声明截断。
    body.truncate(if declared == 0 { BODY_LIMIT.min(body.len()) } else { declared });
    Ok(Request { method, path, query, headers, body })
}

/// 同时认 JSON、`a=b&c=d` 表单和纯文本；不同转发 App 的字段名差异靠别名兜住。
fn fields(body: &[u8], query: &str) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_slice::<serde_json::Value>(body) {
        for (name, value) in map {
            let value = match value {
                serde_json::Value::String(text) => text,
                serde_json::Value::Number(number) => number.to_string(),
                serde_json::Value::Bool(flag) => flag.to_string(),
                _ => continue,
            };
            fields.push((name.to_ascii_lowercase(), value));
        }
    } else {
        let raw = String::from_utf8_lossy(body).into_owned();
        if raw.contains('=') {
            for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                fields.push((decode(name).trim().to_ascii_lowercase(), decode(value)));
            }
        } else if !raw.trim().is_empty() {
            fields.push(("content".into(), raw));
        }
    }
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        fields.push((decode(name).trim().to_ascii_lowercase(), decode(value)));
    }
    fields
}

fn pick<'a>(fields: &'a [(String, String)], names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str()))
}

/// 令牌可以放在请求体字段、`X-Token` 头或 `Authorization: Bearer`。服务端没设令牌时一律拒绝。
fn authorized(fields: &[(String, String)], headers: &[(String, String)], token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let from_field = pick(fields, &["token", "sign", "key", "secret"]).unwrap_or_default();
    if same(from_field, token) {
        return true;
    }
    for (name, value) in headers {
        match name.as_str() {
            "x-token" | "x-sign" | "token" if same(value, token) => return true,
            "authorization" => {
                let bearer = value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer ")).unwrap_or(value);
                if same(bearer.trim(), token) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn same(given: &str, expected: &str) -> bool {
    let given = given.trim();
    if given.len() != expected.len() {
        return false;
    }
    given.bytes().zip(expected.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// 正文整体只由 `[xxx]` / `{{xxx}}` 片段拼成时，判定为「占位符没被替换」。
/// 真实短信即使以 `[淘宝]` 开头，后面也会跟着正文，不会全是占位符。
fn is_placeholder(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 64 {
        return false;
    }
    let mut rest = trimmed;
    let mut parts = 0;
    while !rest.is_empty() {
        let (open, close) = if rest.starts_with("{{") {
            ("{{", "}}")
        } else if rest.starts_with('[') {
            ("[", "]")
        } else {
            return false;
        };
        let after_open = &rest[open.len()..];
        let Some(end) = after_open.find(close) else {
            return false;
        };
        if end == 0 {
            return false;
        }
        rest = &after_open[end + close.len()..];
        parts += 1;
        if parts > 8 {
            return false;
        }
    }
    parts > 0
}

fn when(value: Option<&str>) -> i64 {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return chrono::Utc::now().timestamp();
    };
    if let Ok(number) = value.parse::<i64>() {
        // 毫秒时间戳（13 位）换算成秒。
        return if number > 10_000_000_000 { number / 1000 } else { number };
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%dT%H:%M:%S%z", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(parsed) = chrono::DateTime::parse_from_str(value, format) {
            return parsed.timestamp();
        }
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(value, format) {
            if let Some(local) = parsed.and_local_timezone(chrono::Local).single() {
                return local.timestamp();
            }
        }
    }
    chrono::Utc::now().timestamp()
}

fn decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => match (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    index += 3;
                }
                _ => {
                    out.push(b'%');
                    index += 1;
                }
            },
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn reason(code: u16) -> &'static str {
    match code {
        413 => "内容过大",
        431 => "请求头过大",
        _ => "请求格式不正确",
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let phrase = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {phrase}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_form_and_plain_text_all_land_on_the_same_fields() {
        let json = fields("{\"from\":\"+8613800138000\",\"content\":\"验证码 123456\",\"timestamp\":1730000000000}".as_bytes(), "");
        assert_eq!(pick(&json, &["from"]), Some("+8613800138000"));
        assert_eq!(pick(&json, &["content"]), Some("验证码 123456"));
        assert_eq!(when(pick(&json, &["timestamp"])), 1_730_000_000);

        let form = fields(b"from=10690007&text=%E9%AA%8C%E8%AF%81%E7%A0%81%208642", "");
        assert_eq!(pick(&form, &["text"]), Some("验证码 8642"));

        let plain = fields("【某银行】您的验证码是 556677，5 分钟内有效".as_bytes(), "");
        assert_eq!(pick(&plain, &["content"]).map(|text| text.contains("556677")), Some(true));
    }

    #[test]
    fn token_is_accepted_from_field_header_or_bearer_and_nothing_else() {
        let fields = vec![("token".to_string(), "abc123".to_string())];
        assert!(authorized(&fields, &[], "abc123"));
        assert!(!authorized(&fields, &[], "abc124"));
        assert!(!authorized(&[], &[], "abc123"));
        assert!(authorized(&[], &[("x-token".into(), "abc123".into())], "abc123"));
        assert!(authorized(&[], &[("authorization".into(), "Bearer abc123".into())], "abc123"));
        assert!(!authorized(&[], &[("authorization".into(), "Basic abc123".into())], "abc123"));
        // 服务端没有令牌时，任何请求都不算通过。
        assert!(!authorized(&[("token".into(), String::new())], &[], ""));
    }

    #[test]
    fn chinese_sms_yields_the_code_and_ignores_the_order_number() {
        let cases = [
            ("【京东】验证码 384756，您正在登录，5 分钟内有效", Some("384756")),
            ("【某平台】订单 202411050001 已发货，运费 12 元", None),
            ("Your verification code is 903112. Do not share it.", Some("903112")),
        ];
        for (text, expected) in cases {
            let found = otp::extract("", text, &[]).map(|code| code.text);
            assert_eq!(found.as_deref(), expected, "{text}");
        }
    }

    #[test]
    fn a_real_post_from_the_phone_lands_as_a_message() {
        // 先占一个空闲端口再释放，接着让接收器绑同一个端口。
        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut inbox = Inbox::default();
        inbox.sync(true, port, "tok".into());
        assert!(inbox.status().running, "端口应能监听：{:?}", inbox.status().error);

        let send = |token: &str| {
            let body = format!(
                "{{\"token\":\"{token}\",\"from\":\"10690007\",\"content\":\"验证码 551234，5 分钟内有效\",\"timestamp\":1730000000}}"
            );
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let request = format!(
                "POST /sms HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(request.as_bytes()).unwrap();
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            response
        };

        let rejected = send("wrong");
        assert!(rejected.starts_with("HTTP/1.1 401"), "{rejected}");
        assert!(inbox.try_recv().is_none(), "令牌不对时不能进列表");

        let accepted = send("tok");
        assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");
        assert!(accepted.contains("551234"), "响应里应带上识别出的验证码：{accepted}");

        let mut message = None;
        for _ in 0..50 {
            if let Some(received) = inbox.try_recv() {
                message = Some(received);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let message = message.expect("手机发来的短信应进入通道");
        assert_eq!(message.from, "10690007");
        assert_eq!(message.code.as_deref(), Some("551234"));
        assert_eq!(message.at, 1_730_000_000);
    }

    #[test]
    fn disabled_inbox_does_not_listen_and_reports_port_conflicts() {
        let mut inbox = Inbox::default();
        // 未启用时不监听，也不报错。
        inbox.sync(false, DEFAULT_PORT, "token".into());
        assert_eq!(inbox.status(), Status { running: false, port: DEFAULT_PORT, error: None });
        // 端口被占用时只回错误，不 panic。
        let taken = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = taken.local_addr().unwrap().port();
        inbox.sync(true, port, "token".into());
        assert!(!inbox.status().running);
        assert!(inbox.status().error.is_some());
    }

    #[test]
    fn unsubstituted_placeholders_are_rejected_instead_of_becoming_messages() {
        // 模板没被 App 替换时，正文就是这些字面量。
        assert!(is_placeholder("[sms]"));
        assert!(is_placeholder("{{SMS}}"));
        assert!(is_placeholder("{{FROM}}{{SMS}}{{RECEIVE_TIME}}"));
        assert!(is_placeholder("[from][sms][timestamp]"));
        // 真实短信不能被误判：以括号标签开头的短信，后面还有正文。
        assert!(!is_placeholder("【淘宝】验证码 123456，5 分钟内有效"));
        assert!(!is_placeholder("[淘宝] 您的验证码 123456"));
        assert!(!is_placeholder("您本月流量已用 8.2G"));
        assert!(!is_placeholder(""));
    }
}
