//! The "no connection" window: shows where the way to Discord breaks and looks for a way that works.
//! The token is never sent from here and the system is not changed; the only thing saved is a proxy that was
//! just seen to work (or one typed in), in Nimbus's own settings.

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, VecModel, Weak};
use tokio::net::TcpStream;

use crate::discord;
use crate::i18n::{t, tf};
use crate::{copy_text, on_ui, AppWindow, NetStep, NetTexts};

const WAITING: i32 = 0;
const FINE: i32 = 1;
const BROKEN: i32 = 2;

const SEARCHING_HOST: &str = "gateway.discord.gg:443";

/// Where reports go when the person agrees. Given at build time, so that the public source has no address;
/// a build without it never offers to send.
const REPORT_URL: Option<&str> = option_env!("NIMBUS_REPORT_URL");

/// Reports are only sent over an encrypted connection.
fn report_address(url: Option<&'static str>) -> Option<&'static str> {
    url.filter(|url| url.starts_with("https://"))
}

/// What "Copy report" gives: the last check, without anything about the account.
static REPORT: Mutex<String> = Mutex::new(String::new());

async fn within<T>(seconds: u64, work: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::time::timeout(Duration::from_secs(seconds), work).await.map_err(|_| format!("no answer in {seconds} s"))?
}

async fn internet() -> Result<String, String> {
    for address in ["1.1.1.1:443", "8.8.8.8:443"] {
        if TcpStream::connect(address).await.is_ok() {
            return Ok(String::new());
        }
    }
    Err("no connection to the internet".to_string())
}

async fn address() -> Result<String, String> {
    let found = tokio::net::lookup_host(SEARCHING_HOST).await.map_err(|e| e.to_string())?.next().ok_or("no address")?;
    if found.ip().is_loopback() || found.ip().is_unspecified() {
        return Err(format!("the name leads to {} (blocked by DNS)", found.ip()));
    }
    Ok(found.ip().to_string())
}

async fn reach() -> Result<String, String> {
    TcpStream::connect(SEARCHING_HOST).await.map(|_| String::new()).map_err(|e| e.to_string())
}

/// reqwest's own message says little; the cause (a certificate, a reset) is further down.
fn whole_error(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        text = format!("{text}: {next}");
        cause = next.source();
    }
    text
}

async fn site() -> Result<String, String> {
    let answer = discord::Api::new(String::new()).http.get("https://discord.com/api/v10/gateway").send().await.map_err(|e| whole_error(&e))?;
    let line = format!("HTTP {}", answer.status().as_u16());
    if answer.status().is_success() {
        Ok(line)
    } else {
        Err(line)
    }
}

/// What the five checks (internet, name, reach, site, gateway) mean, in words.
fn verdict(results: [bool; 5], certificate: bool) -> String {
    let [internet, _, _, site, gateway] = results;
    if site && gateway {
        t("Связь с Discord есть. Если что-то всё равно не работает, скопируй отчёт и отправь разработчику.")
    } else if certificate {
        t("Discord не прошёл проверку сертификата. Проверь дату и время на компьютере и отключи проверку HTTPS в антивирусе.")
    } else if !internet && !site {
        t("Похоже, на компьютере нет интернета. Проверь подключение к сети.")
    } else {
        t("Discord закрыт в этой сети. Нажми «Исправить»: Nimbus поищет рабочий прокси на этом компьютере.")
    }
}

async fn diagnose(show: impl Fn(&[NetStep])) -> (String, String) {
    let mut steps: Vec<NetStep> = [t("Интернет"), t("Адрес Discord"), t("Доступ к серверу"), t("Сайт Discord"), t("Постоянное подключение")]
        .map(|name| NetStep { name: name.into(), state: WAITING, detail: "".into() })
        .to_vec();
    show(&steps);
    let mut results = [false; 5];
    let mut certificate = false;
    for i in 0..steps.len() {
        let outcome = match i {
            0 => within(6, internet()).await,
            1 => within(6, address()).await,
            2 => within(8, reach()).await,
            3 => within(20, site()).await,
            _ => within(40, async { discord::test_gateway().await.map(|_| String::new()) }).await,
        };
        results[i] = outcome.is_ok();
        certificate |= outcome.as_ref().is_err_and(|e| e.to_lowercase().contains("certificate"));
        steps[i].state = if results[i] { FINE } else { BROKEN };
        steps[i].detail = outcome.unwrap_or_else(|e| e).into();
        show(&steps);
    }
    let said = verdict(results, certificate);
    let mut report = format!("Nimbus {}\n", env!("CARGO_PKG_VERSION"));
    for step in &steps {
        report += &format!("{}: {} {}\n", step.name, if step.state == FINE { "ok" } else { "FAILED" }, step.detail);
    }
    report += &format!("proxy found: {:?}\nsaved proxy: {:?}\n{said}\n", discord::proxy_candidates().await, discord::own_proxy());
    (said, report)
}

/// Starts this copy again a moment after it has gone: one copy runs at a time, and the proxy is taken at the start.
fn restart() -> bool {
    use std::os::windows::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else { return false };
    let started = std::process::Command::new("cmd")
        .raw_arg(format!(r#"/c ping -n 3 127.0.0.1 >nul & start "" "{}""#, exe.display()))
        .creation_flags(0x0800_0000)
        .spawn();
    if started.is_ok() {
        std::process::exit(0);
    }
    false
}

fn say(ui: &Weak<AppWindow>, text: String) {
    on_ui(ui, move |ui| ui.set_net_verdict(text.into()));
}

fn use_proxy(ui: &Weak<AppWindow>, proxy: Option<String>) {
    discord::save_proxy(proxy.as_deref());
    if !restart() {
        say(ui, t("Сохранено. Перезапусти Nimbus."));
    }
}

async fn repair(ui: Weak<AppWindow>) {
    say(&ui, t("Ищу рабочий путь…"));
    let saved = discord::own_proxy();
    if discord::test_direct().await.is_ok() {
        if saved.is_some() {
            use_proxy(&ui, None);
        } else {
            say(&ui, t("Прямое подключение работает, Nimbus подключится сам."));
        }
        return;
    }
    for proxy in discord::proxy_candidates().await {
        say(&ui, tf("Проверяю {}…", &[&proxy]));
        if discord::test_proxy(&proxy).await.is_ok() {
            if saved.as_ref() == Some(&proxy) {
                say(&ui, t("Этот прокси работает, Nimbus подключится сам."));
            } else {
                use_proxy(&ui, Some(proxy));
            }
            return;
        }
    }
    say(&ui, t("Рабочий путь не найден. Включи VPN на весь трафик или впиши адрес прокси и нажми «Применить»."));
}

async fn apply(ui: Weak<AppWindow>, text: String) {
    if text.trim().is_empty() {
        use_proxy(&ui, None);
        return;
    }
    let proxy = match discord::parse_proxy(&text) {
        Ok(proxy) => proxy,
        Err(e) => return say(&ui, e),
    };
    say(&ui, tf("Проверяю {}…", &[&proxy]));
    match discord::test_proxy(&proxy).await {
        Ok(()) => use_proxy(&ui, Some(proxy)),
        Err(e) => say(&ui, tf("Этот прокси не подключился: {}", &[&e])),
    }
}

/// Only after the person said yes, and only the text they were shown.
async fn send(ui: Weak<AppWindow>) {
    let Some(url) = report_address(REPORT_URL) else { return };
    let sent = |text: String| on_ui(&ui, move |ui| ui.set_net_sent(text.into()));
    sent(t("Отправляю…"));
    let report = REPORT.lock().unwrap().clone();
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().map_err(|e| e.to_string());
    let body = serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "report": report });
    let answer = match client {
        Ok(client) => client.post(url).json(&body).send().await.and_then(|answer| answer.error_for_status()).map_err(|e| whole_error(&e)),
        Err(e) => Err(e),
    };
    match answer {
        Ok(answer) => {
            let id = answer.json::<serde_json::Value>().await.ok().and_then(|v| v["id"].as_str().map(str::to_string));
            sent(id.map_or_else(|| t("Отправлено."), |id| tf("Отправлено. Номер отчёта: {}", &[&id])));
        }
        Err(e) => sent(tf("Не удалось отправить: {}", &[&e])),
    }
}

/// One job at a time, on its own thread: the sign-in screen has no client running yet to hand it to.
fn begin<Work: Future<Output = ()>>(ui: &Weak<AppWindow>, job: impl FnOnce(Weak<AppWindow>) -> Work + Send + 'static) {
    let Some(window) = ui.upgrade() else { return };
    if window.get_net_busy() {
        return;
    }
    window.set_net_busy(true);
    let ui = ui.clone();
    std::thread::spawn(move || {
        tokio::runtime::Runtime::new().expect("failed to start the tokio runtime").block_on(job(ui.clone()));
        on_ui(&ui, |ui| ui.set_net_busy(false));
    });
}

fn texts() -> NetTexts {
    NetTexts {
        banner: t("Нет подключения к Discord").into(),
        fix: t("Починить").into(),
        title: t("Диагностика подключения").into(),
        repair: t("Исправить").into(),
        copy: t("Скопировать отчёт").into(),
        close: t("Закрыть").into(),
        proxy_label: t("Свой прокси, если он есть").into(),
        apply: t("Применить").into(),
        placeholder: "127.0.0.1:1080, socks5://127.0.0.1:1080".into(),
        ask: t("Отправить эту диагностику разработчикам?").into(),
        ask_note: t("Уйдёт только этот текст, токена и данных аккаунта в нём нет. Сервер увидит твой IP-адрес, как любой сайт.").into(),
        send: t("Отправить").into(),
        dont_send: t("Не отправлять").into(),
    }
}

/// The line over the window that offers to look into it.
pub fn set_down(ui: &Weak<AppWindow>, down: bool) {
    on_ui(ui, move |ui| {
        ui.set_net_texts(texts());
        ui.set_net_down(down);
    });
}

pub fn wire(ui: &AppWindow) {
    // Demo windows only show made-up results: no network.
    let demo = std::env::args().any(|a| a == "--demo");
    let weak = ui.as_weak();
    ui.on_net_check(move || {
        let Some(window) = weak.upgrade().filter(|_| !demo) else { return };
        window.set_net_proxy(discord::own_proxy().unwrap_or_default().into());
        window.set_net_verdict(t("Проверяю подключение…").into());
        window.set_net_asking(false);
        window.set_net_sent("".into());
        begin(&weak, |ui| async move {
            let shown = ui.clone();
            let (said, report) = diagnose(move |steps| {
                let steps = steps.to_vec();
                on_ui(&shown, move |ui| ui.set_net_steps(ModelRc::new(VecModel::from(steps))));
            })
            .await;
            *REPORT.lock().unwrap() = report.clone();
            say(&ui, said);
            if report_address(REPORT_URL).is_some() {
                on_ui(&ui, move |ui| {
                    ui.set_net_report(report.into());
                    ui.set_net_asking(true);
                });
            }
        });
    });
    let weak = ui.as_weak();
    ui.on_net_repair(move || {
        if !demo {
            begin(&weak, repair);
        }
    });
    let weak = ui.as_weak();
    ui.on_net_apply(move |text| {
        if !demo {
            let text = text.to_string();
            begin(&weak, |ui| apply(ui, text));
        }
    });
    ui.on_net_copy(|| copy_text(&REPORT.lock().unwrap()));
    let weak = ui.as_weak();
    ui.on_net_send(move || {
        if !demo {
            begin(&weak, send);
        }
    });
}

/// `--net-check[=proxy]`: the same check without a window, into the log. Nothing is sent but the connections themselves.
pub async fn report(proxy: Option<String>) -> String {
    let (_, mut report) = diagnose(|_| {}).await;
    if let Some(proxy) = proxy {
        report += &format!("{proxy}: {:?}\n", discord::test_proxy(&proxy).await);
    }
    report
}

/// `--demo-netdown`: the line; `--demo-netcheck`: the window, with made-up results.
pub fn demo(ui: &AppWindow) {
    let flag = |name: &str| std::env::args().any(|a| a == name);
    if flag("--demo-netdown") || flag("--demo-netcheck") {
        ui.set_net_texts(texts());
        ui.set_net_down(true);
    }
    if flag("--demo-netcheck") {
        let step = |name: String, state: i32, detail: &str| NetStep { name: name.into(), state, detail: detail.into() };
        let steps = vec![
            step(t("Интернет"), FINE, ""),
            step(t("Адрес Discord"), FINE, "192.0.2.10"),
            step(t("Доступ к серверу"), BROKEN, "no answer in 8 s"),
            step(t("Сайт Discord"), BROKEN, "no answer in 20 s"),
            step(t("Постоянное подключение"), WAITING, ""),
        ];
        ui.set_net_steps(ModelRc::new(VecModel::from(steps)));
        ui.set_net_verdict(verdict([true, true, false, false, false], false).into());
        ui.set_net_report("Nimbus 0.0.0\nInternet: ok\nReaching the server: FAILED no answer in 8 s".into());
        ui.set_net_asking(true);
        ui.set_net_open(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_go_only_over_https() {
        assert_eq!(report_address(Some("https://reports.example/v1")), Some("https://reports.example/v1"));
        assert_eq!(report_address(Some("http://reports.example/v1")), None);
        assert_eq!(report_address(None), None);
    }

    #[test]
    fn the_words_follow_what_failed() {
        assert!(verdict([true; 5], false).starts_with("Связь с Discord есть"));
        assert!(verdict([true, true, true, false, false], true).contains("сертификата"));
        assert!(verdict([false, false, false, false, false], false).contains("нет интернета"));
        assert!(verdict([true, true, false, false, false], false).contains("закрыт в этой сети"));
        assert!(verdict([true, true, true, true, false], false).contains("закрыт в этой сети"), "the site opens, the gateway doesn't");
    }
}
