//! HTTP 传输层：监听、鉴权、路由。
//!
//! 这一层只认「请求进来、响应出去」，不认任何测试语义。鉴权放在这里而不是
//! 各个 handler 里，是因为漏掉一个 handler 的代价是整台机器的执行权限。

use super::*;

/// 绑定地址是否只有本机能连上。
///
/// 判据放宽一点没关系（把某个实际可路由的地址误判成回环才危险，反过来
/// 只是多要一个 token），所以这里只认明确的回环写法。
pub(crate) fn bind_is_loopback(bind: &str) -> bool {
    let bind = bind.trim();
    bind.eq_ignore_ascii_case("localhost")
        || bind == "::1"
        || bind == "[::1]"
        || bind
            .parse::<std::net::Ipv4Addr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// 把绑定地址和端口拼成 `Server::http` 能解析的监听地址。
///
/// 裸 IPv6 必须补方括号，否则 `"::1:28800"` 里的冒号无从区分地址和端口，
/// 解析直接失败——而 `bind_is_loopback` 是认 `"::1"` 的，不补的话
/// 「判定放行 → 监听失败」这条路走得通，人只会看到一句莫名其妙的启动错误。
pub(crate) fn listen_addr(bind: &str, port: u16) -> String {
    let bind = bind.trim();
    if bind.contains(':') && !bind.starts_with('[') {
        format!("[{bind}]:{port}")
    } else {
        format!("{bind}:{port}")
    }
}

/// 把绑定地址拼成**能在浏览器里打开**的地址。
///
/// 监听地址不等于访问地址：`0.0.0.0` 和 `::` 是「所有网卡」的通配写法，不是
/// 一个能连的目的地址。此前打印和自动弹出的都是监听地址原文，于是 `--ui-bind
/// 0.0.0.0` 弹出来的是 `http://0.0.0.0:28800?token=…`——Chrome 133 起为堵
/// 「0.0.0.0 day」直接拦掉对该地址的请求，其余浏览器靠「碰巧路由到回环」才打
/// 得开；而口令就在那串 URL 里，人得先看懂要把主机名换掉才能进得去。
///
/// 通配地址换成对应的回环，其余原样保留（绑到某块网卡的 IP 时，那个 IP 本来
/// 就是该用的访问地址）。
pub(crate) fn display_addr(bind: &str, port: u16) -> String {
    let host = match bind.trim() {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        other => other,
    };
    listen_addr(host, port)
}

/// 绑定地址是否是「所有网卡」的通配写法。
pub(crate) fn bind_is_wildcard(bind: &str) -> bool {
    matches!(bind.trim(), "0.0.0.0" | "::" | "[::]")
}

/// 同时处理请求的线程数。
///
/// 单线程轮询在这里是会被人看见的卡顿：`/api/local` 要跑一次 `scan_host()`，
/// 在 Windows 上会拉起 ipconfig/netsh，一到两秒；这期间页面每秒一次的日志轮询
/// 和速率采样轮询全在排队，日志停住、曲线断一截。控制台的共享状态本来就都在
/// Mutex 后面，并发处理不需要额外的同步。
pub(super) const UI_WORKERS: usize = 4;

/// 取请求循环的空转周期：没有请求进来时，隔这么久回头查一次取消标志。
/// 取小一点没有代价（`recv_timeout` 超时是纯等待），但要足够小，
/// 让 Ctrl+C 之后的退出感觉是「立刻」。
pub(super) const SHUTDOWN_POLL: Duration = Duration::from_millis(200);

/// 收到 Ctrl+C 之后是否该收摊。
///
/// 一轮测试正在跑时**不能**退：那次 Ctrl+C 的语义是「优雅结束当前单元并出报告」，
/// 控制台进程得活到 `run_master()` 把报告写完。等它收完尾、`running` 落回 false，
/// 下一拍才轮到控制台自己退出——和命令行主控按一次 Ctrl+C 的行为对齐。
pub(super) fn should_shut_down(cancelled: bool, run_in_flight: bool) -> bool {
    cancelled && !run_in_flight
}

/// 停掉全部监控会话。进程退出前调用，也可被显式关停复用。
pub(super) fn stop_all_monitors(console: &Arc<Console>) {
    let sessions: Vec<String> = lock_recover(&console.monitors).keys().cloned().collect();
    for session in sessions {
        let body = serde_json::json!({ "session": session }).to_string();
        let _ = api_monitor_stop(console, &body);
    }
}

/// 一条工作线程的取请求-处理循环。`recv_timeout` 在多线程间是安全的，
/// tiny_http 自己排队分发。
///
/// 用 `recv_timeout` 而不是 `recv()`：后者没有出口，取消标志永远查不到。
pub(super) fn serve(server: &Server, console: &Arc<Console>, shutdown: &AtomicBool) {
    while !shutdown.load(Ordering::SeqCst) {
        match server.recv_timeout(SHUTDOWN_POLL) {
            Ok(Some(request)) => handle(request, console),
            Ok(None) => {
                if should_shut_down(
                    crate::cancel::is_shutdown_requested(),
                    console.running.load(Ordering::SeqCst),
                ) {
                    shutdown.store(true, Ordering::SeqCst);
                }
            }
            Err(_) => break,
        }
    }
}

pub(super) fn header(name: &'static [u8], value: &'static [u8]) -> Header {
    Header::from_bytes(name, value).expect("static response header")
}

pub(super) fn json_response(body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_header(header(b"Content-Type", b"application/json; charset=utf-8"))
        .with_header(header(b"Cache-Control", b"no-store"))
        .with_header(header(b"X-Content-Type-Options", b"nosniff"))
}

/// 报告包的下载响应。
///
/// `Content-Disposition: attachment` 让浏览器直接落盘而不是尝试渲染——
/// zip 里是整个 run 目录，解开就是完整可读的报告。
pub(super) fn bundle_response<R: std::io::Read>(run_id: &str, body: Response<R>) -> Response<R> {
    // `run_id` 是 `runs/` 下真实存在的目录名。工具自己建的目录全是
    // `run_<数字>_<进程号>` 形状，但**目录名是文件系统给的，不是这里能保证的**
    // ——有人手动建/改过名字，就可能带引号或换行，直接拼进 header 就是注入面。
    // 所以按白名单转写：只留 ASCII 字母数字、`-`、`_`、`.`，其余换成 `_`。
    let safe: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = if safe.is_empty() { "run".into() } else { safe };
    let disposition = format!("attachment; filename=\"{safe}.zip\"");
    // tiny_http 会自己按数据长度补 Content-Length，不用手写。
    let mut response = body
        .with_header(header(b"Content-Type", b"application/zip"))
        .with_header(header(b"Cache-Control", b"no-store"))
        .with_header(header(b"X-Content-Type-Options", b"nosniff"));
    if let Ok(value) =
        tiny_http::Header::from_bytes(&b"Content-Disposition"[..], disposition.as_bytes())
    {
        response.add_header(value);
    }
    response
}

/// 控制台会话 cookie 的名字。前端 `api/client.ts` 按同一个名字读。
pub(super) const SESSION_COOKIE: &str = "cpe_ui_session";

/// 交付页面时同时把口令发成一枚会话 cookie。
///
/// # 补的是哪个洞
///
/// 前端拿到口令后会把地址栏的 `?token=` 抹掉（那是有意的：地址栏里的口令会进
/// 浏览器历史、会被截图带走、会在复制链接给同事时一起发出去）。但这样一来，
/// **F5 刷新发出的 `GET /` 就什么凭据都不带了**——鉴权在路由之前，于是刷新
/// 直接撞 401，页面变成一句「未认证」的 JSON。口令明明还在，只是那一次导航
/// 请求带不上它：浏览器不会给地址栏导航加自定义头。
///
/// # 为什么这不违反「鉴权先于路由」
///
/// cookie 只是**文档请求**的第三种凭据形式，页面依然要先认证才发得出去。
/// 而且它**只对 `GET /` 生效**：任何 `/api/*` 都仍然只认 `X-CPE-Token` /
/// `Authorization: Bearer` / `?token=`，光有 cookie 一律 401。CSRF 那道门因此
/// 原样保留——别的站点能让浏览器去导航 `GET /`，但拿不到响应（CORS 挡读、
/// `frame-ancestors 'none'` 挡框），更发不出任何一个会执行动作的 API 调用。
/// `SameSite=Strict` 让它连跨站导航都不发。
///
/// # 为什么不加 HttpOnly
///
/// 页面要能读它：sessionStorage 是**按标签页**的，把控制台地址复制到新标签打开
/// 时那里是空的。没有可读的 cookie，新标签会「页面打开了、每个 API 都 401」——
/// 比刷新报错更难懂。代价是页面脚本能读到口令，但页面脚本本来就在用这个口令
/// 调 API，拿不到 cookie 也照样能发请求，所以这里没有新增可达的能力。
///
/// 不设 `Secure`：控制台是明文 HTTP（§13 已登记，口令在同网段本来就是明文往返）。
/// 不设 `Max-Age`：会话 cookie，关掉浏览器就没了，与 sessionStorage 的预期一致。
pub(super) fn page_response(ui_token: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_string(PAGE)
        .with_header(header(b"Content-Type", b"text/html; charset=utf-8"))
        .with_header(header(b"Cache-Control", b"no-store"))
        .with_header(header(b"X-Content-Type-Options", b"nosniff"))
        .with_header(header(b"Referrer-Policy", b"no-referrer"))
        .with_header(header(
            b"Content-Security-Policy",
            b"default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ));
    if !ui_token.is_empty() {
        // 值走 urlencode：口令是命令行给的任意字符串，原样塞进 cookie 会被
        // `;` 或空格截断，表现是「刷新有时候好使有时候不好使」。
        let cookie = format!(
            "{SESSION_COOKIE}={}; Path=/; SameSite=Strict",
            urlencode(ui_token)
        );
        if let Ok(value) = Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes()) {
            response.add_header(value);
        }
    }
    response
}

/// 从 `Cookie` 头里取控制台会话口令。
pub(super) fn cookie_token(request: &Request) -> Option<String> {
    let raw = header_value(request, "Cookie")?;
    raw.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == SESSION_COOKIE).then(|| urldecode(value.trim()))
    })
}

/// 自定义头会让跨站 fetch 先触发 CORS 预检；本服务不开放 CORS，因此网页不能
/// 趁用户开着本地控制台时从别的站点静默发起测试。原生程序仍可显式带头调用。
pub(super) fn has_console_request_header(request: &Request) -> bool {
    request
        .headers()
        .iter()
        .any(|h| h.field.equiv("X-CPE-Console") && h.value.as_str() == "1")
}

pub(super) fn header_value(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

/// 只处理会出现在 token 里的那些字符；这里不需要一个通用的 URL 编码器。
pub(super) fn urlencode(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// 请求是否带对了控制台口令。
///
/// 三种带法都认：浏览器第一次打开只能靠查询串（地址栏里输不了请求头），
/// 页面之后的 API 调用走请求头，`Authorization: Bearer` 则是为了和 agent
/// 协议侧保持一致、也方便 curl 复现问题。
pub(crate) fn request_is_authorized(
    token: &str,
    query: &str,
    header_token: Option<&str>,
    bearer: Option<&str>,
) -> bool {
    if token.is_empty() {
        return true;
    }
    if header_token.is_some_and(|value| secret_eq(value, token))
        || bearer.is_some_and(|value| secret_eq(value, token))
    {
        return true;
    }
    query
        .split('&')
        .filter_map(|kv| kv.strip_prefix("token="))
        .any(|value| secret_eq(&urldecode(value), token))
}

/// 文档请求（`GET /`）的鉴权：在三种带法之外**额外**认会话 cookie。
///
/// 只有页面这一条路径认它。API 走 [`request_is_authorized`]，光带 cookie 一律
/// 401——CSRF 那道门就是靠这个分岔保住的，理由写在 [`page_response`] 上。
pub(crate) fn page_request_is_authorized(
    token: &str,
    query: &str,
    header_token: Option<&str>,
    bearer: Option<&str>,
    cookie: Option<&str>,
) -> bool {
    request_is_authorized(token, query, header_token, bearer)
        || (!token.is_empty() && cookie.is_some_and(|value| secret_eq(value, token)))
}

/// 口令比较，不因第一个不同的字节提前返回。
///
/// `--ui-bind` 之后控制台就在局域网上了，而这里既没有失败限速也没有锁定：
/// 普通的 `==` 会在第一个不匹配的字节上返回，攻击者可以不限次数地量响应时间，
/// 一个字节一个字节把口令试出来。长度仍然会泄露，那是口令强度的事，
/// 不是能逐位收敛的信道。
pub(super) fn secret_eq(given: &str, expected: &str) -> bool {
    let (given, expected) = (given.as_bytes(), expected.as_bytes());
    if given.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in given.iter().zip(expected) {
        diff |= a ^ b;
    }
    diff == 0
}

pub(super) fn urldecode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            // 按**字节**取那两位十六进制，不能对 &str 下标切片：`%` 后面
            // 跟着多字节字符时（比如 "%中"），字符串切片会切在字符中间
            // 直接 panic——而这段输入来自网络，谁都能构造。
            b'%' if idx + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[idx + 1..idx + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    Some(byte) => {
                        out.push(byte);
                        idx += 3;
                    }
                    None => {
                        out.push(b'%');
                        idx += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                idx += 1;
            }
            byte => {
                out.push(byte);
                idx += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(super) fn handle(mut request: Request, console: &Arc<Console>) {
    let path = request.url().split('?').next().unwrap_or("/").to_string();
    let query = request
        .url()
        .split_once('?')
        .map(|(_, q)| q.to_string())
        .unwrap_or_default();
    let trusted_post = *request.method() != Method::Post || has_console_request_header(&request);
    let mut body = String::new();
    if *request.method() == Method::Post {
        let mut limited = request.as_reader().take(MAX_BODY_BYTES);
        let _ = limited.read_to_string(&mut body);
    }

    // 鉴权先于一切，页面本身也不例外：页面里带着给 API 用的口令，
    // 放行未认证的 GET / 等于把口令发给任何来问的人。
    let header_token = header_value(&request, "X-CPE-Token");
    let bearer = header_value(&request, "Authorization").and_then(|value| {
        value
            .strip_prefix("Bearer ")
            .map(|token| token.trim().to_string())
    });
    // 页面（文档请求）额外认会话 cookie：抹掉地址栏 `?token=` 之后，F5 发出的
    // 导航请求什么凭据都带不上，撞 401 的是**刷新**这个最普通的动作。
    // API 不认 cookie，见 `page_request_is_authorized` / `page_response`。
    let is_page = path == "/" || path == "/index.html";
    let authorized = if is_page {
        page_request_is_authorized(
            &console.ui_token,
            &query,
            header_token.as_deref(),
            bearer.as_deref(),
            cookie_token(&request).as_deref(),
        )
    } else {
        request_is_authorized(
            &console.ui_token,
            &query,
            header_token.as_deref(),
            bearer.as_deref(),
        )
    };
    if !authorized {
        let body = crate::protocol::err_json(
            "未认证：控制台已启用访问口令，请用启动时打印的完整地址（带 ?token=）打开",
        );
        let _ = request.respond(json_response(body).with_status_code(401));
        return;
    }

    if is_page {
        let _ = request.respond(page_response(&console.ui_token));
        return;
    }

    let is_post = *request.method() == Method::Post;
    let is_get = *request.method() == Method::Get;

    // 报告打包下载。**在 JSON 分支之前拦下**：它回的是二进制流，不是 `Resp`。
    //
    // 这是远程访问者取回报告的唯一通道（§13.3）：`/api/open-report` 只在跑
    // 控制台的那台机器上调系统程序打开，`--ui-bind` 之后远程用户永远拿不到
    // 报告——而报告是这个工具的产物本身。
    //
    // 不把报告目录当静态站点服务出来，是因为报告 HTML 里的截图/CSV 是相对路径
    // 子资源，浏览器加载它们时不带自定义头——撞的是和控制台页面同一堵
    // 「鉴权先于路由」的墙（ADR-5 已否决同构方案）。
    if is_get {
        if let Some(id) = path
            .strip_prefix("/api/runs/")
            .and_then(|rest| rest.strip_suffix("/bundle.zip"))
        {
            // 白名单式解析：只认 `runs/` 下已经存在的目录名，不做路径拼接。
            match runs::resolve_run_dir(id) {
                Some(dir) => match runs::build_bundle(&dir, id) {
                    // `bundle` 是临时 zip 的 RAII 守卫：`respond` 返回时响应连同
                    // 它持有的 File 已经析构，这时删才不会在 Windows 上撞
                    // 「文件正被占用」。所以守卫必须活到 respond 之后。
                    Ok(bundle) => {
                        match std::fs::File::open(&bundle.path) {
                            Ok(file) => {
                                let _ =
                                    request.respond(bundle_response(id, Response::from_file(file)));
                            }
                            Err(error) => {
                                let message = format!("打包失败: {error}");
                                let _ = request
                                    .respond(json_response(crate::protocol::err_json(&message)));
                            }
                        }
                        drop(bundle);
                        return;
                    }
                    Err(error) => {
                        let message = format!("打包失败: {error}");
                        let _ = request.respond(json_response(crate::protocol::err_json(&message)));
                        return;
                    }
                },
                None => {
                    let _ = request.respond(json_response(crate::protocol::err_json(
                        "找不到这个运行目录",
                    )));
                    return;
                }
            }
        }
    }
    let out = if !trusted_post {
        Err("拒绝跨站请求：缺少 X-CPE-Console 请求头".to_string())
    } else if is_get && path == "/api/bootstrap" {
        api_bootstrap(console)
    } else if is_get && path == "/api/local" {
        api_local()
    } else if is_post && path == "/api/connect" {
        api_connect(console, &body)
    } else if is_post && path == "/api/plan" {
        api_plan(console, &body)
    } else if is_post && path == "/api/config" {
        api_config(console, &body)
    } else if is_post && path == "/api/import" {
        api_import(console, &body)
    } else if is_post && path == "/api/run" {
        api_run(console, &body)
    } else if is_post && path == "/api/stop" {
        api_stop(console)
    } else if is_post && path == "/api/open-report" {
        api_open_report(console)
    } else if is_get && path == "/api/progress" {
        Ok(api_progress(console, &query))
    } else if is_get && path == "/api/runs" {
        runs::api_runs()
    } else if is_post && path == "/api/runs/request" {
        runs::api_run_request(&body)
    } else if is_post && path == "/api/runs/report" {
        runs::api_run_replay(console, &body)
    } else if is_post && path == "/api/monitor/start" {
        api_monitor_start(console, &body)
    } else if is_post && path == "/api/monitor/samples" {
        api_monitor_samples(console, &body)
    } else if is_post && path == "/api/monitor/stop" {
        api_monitor_stop(console, &body)
    } else {
        Err("未知接口或请求方法".to_string())
    };
    let body = match out {
        Ok(value) => crate::protocol::ok_json(value),
        Err(error) => crate::protocol::err_json(&error),
    };
    let _ = request.respond(json_response(body));
}

pub(super) fn post<T: serde::de::DeserializeOwned>(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    token: &str,
) -> Result<T, String> {
    let (status, text) =
        http_client::post_json_auth(host, port, path, body, token, Duration::from_secs(60))?;
    if status == 401 {
        return Err("agent 返回 401：已启用令牌认证，请填写相同的 token".into());
    }
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    let resp: Resp<T> = serde_json::from_str(&text).map_err(|e| format!("响应解析失败: {e}"))?;
    if !resp.ok {
        return Err(resp.error.unwrap_or_else(|| "未知错误".into()));
    }
    resp.data.ok_or_else(|| "响应缺 data".into())
}
