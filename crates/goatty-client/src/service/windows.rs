//! Windows per-user Scheduled Task backend (`goattyd`).
//!
//! Windows Services (SCM) run in Session 0 (isolated from the user's
//! credentials, environment, and GUI askpass) and require Administrator UAC to
//! install, violating goatty's no-admin rule. Instead, `goattyd service install`
//! registers a per-user Scheduled Task with a `LogonTrigger` (`LeastPrivilege`,
//! unlimited runtime, allowed on battery, hidden window, logs to
//! `%LOCALAPPDATA%\goatty\goattyd.log`).
//!
//! Because Windows has no `execve` and ConPTY pseudo-console handles (`HPCON`)
//! are tied to their owning process, stopping or restarting `goattyd` ends its
//! running sessions.

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::{Command, Output, Stdio};

#[cfg(windows)]
use anyhow::{Context, Result, bail};

#[cfg(windows)]
use super::{Installed, Status};

/// Scheduled Task name registered under the user's Task Scheduler root (`\goattyd`).
pub const WINDOWS_TASK_NAME: &str = "goattyd";

/// Marker stored in `<Description>` so we never overwrite or remove a
/// user-authored task with the same name.
const DESCRIPTION: &str = "Written by `goattyd service install`; `goattyd service uninstall` removes it.";

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp + 1..];
        let Some(semi) = rest.find(';') else {
            out.push('&');
            break;
        };
        let entity = &rest[..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(ch) => out.push(ch),
            None => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
    out
}

fn quote_arg(s: &str) -> String {
    if s.chars().any(|c| c.is_whitespace() || c == '"') { format!("\"{}\"", s.replace('"', "\\\"")) } else { s.to_string() }
}

/// Render the Windows Task Scheduler 1.2 XML definition for the per-user
/// `goattyd` background task.
///
/// `user_id` should be `DOMAIN\User` or `User` so `LogonTrigger` scopes to the
/// current user without requiring elevation.
pub fn render_task_xml(exe: &Path, log_path: &Path, user_id: Option<&str>) -> String {
    let exe_escaped = xml_escape(&exe.to_string_lossy());
    let args = format!("serve --log {}", quote_arg(&log_path.to_string_lossy()));
    let args_escaped = xml_escape(&args);
    let user_tag = match user_id.filter(|u| !u.is_empty()) {
        Some(u) => format!("\n      <UserId>{}</UserId>", xml_escape(u)),
        None => String::new(),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{DESCRIPTION}</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>{user_tag}
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">{user_tag}
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe_escaped}</Command>
      <Arguments>{args_escaped}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

#[cfg_attr(not(windows), allow(dead_code))]
fn is_goatty_task(xml: &str) -> bool {
    xml.contains(DESCRIPTION)
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let after = xml.split_once(&open)?.1;
    let body = after.split_once(&close)?.0;
    Some(xml_unescape(body.trim()))
}

/// Extract the `<Command>` executable path from a Task Scheduler XML document.
#[cfg_attr(not(windows), allow(dead_code))]
fn task_program(xml: &str) -> Option<PathBuf> {
    extract_tag(xml, "Command").filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// Human-readable command line from `<Command>` + `<Arguments>`.
#[cfg_attr(not(windows), allow(dead_code))]
fn task_exec_display(xml: &str) -> Option<String> {
    let cmd = extract_tag(xml, "Command").filter(|s| !s.is_empty())?;
    let args = extract_tag(xml, "Arguments").unwrap_or_default();
    let cmd_q = quote_arg(&cmd);
    if args.is_empty() { Some(cmd_q) } else { Some(format!("{cmd_q} {args}")) }
}

/// Parse `schtasks /Query /TN goattyd /FO LIST` output to determine whether
/// the task is currently running.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_schtasks_running(out: &str) -> bool {
    for line in out.lines() {
        if let Some((key, val)) = line.split_once(':') {
            let k = key.trim();
            let v = val.trim();
            if k.eq_ignore_ascii_case("Status") && v.eq_ignore_ascii_case("Running") {
                return true;
            }
        }
    }
    false
}

/// `%LOCALAPPDATA%\goatty\goattyd.log`.
#[cfg(windows)]
pub fn log_path() -> Option<PathBuf> {
    log_path_from(|k| std::env::var_os(k))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn log_path_from(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(local) = env("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(local).join("goatty").join("goattyd.log"));
    }
    goatty_proto::home_dir_from(true, env).map(|h| h.join("AppData").join("Local").join("goatty").join("goattyd.log"))
}

#[cfg(windows)]
fn current_user_id() -> Option<String> {
    let user = std::env::var("USERNAME").ok().filter(|s| !s.is_empty())?;
    match std::env::var("USERDOMAIN").ok().filter(|s| !s.is_empty()) {
        Some(domain) => Some(format!("{domain}\\{user}")),
        None => Some(user),
    }
}

#[cfg(windows)]
pub fn unit_dir() -> Option<PathBuf> {
    log_path().and_then(|p| p.parent().map(Path::to_path_buf))
}

#[cfg(windows)]
pub fn unit_socket_path() -> Option<PathBuf> {
    Some(goatty_proto::default_windows_pipe_path())
}

#[cfg(windows)]
fn schtasks_raw(args: &[&str]) -> Result<Output> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    Command::new("schtasks.exe")
        .args(args)
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("couldn't run schtasks.exe")
}

#[cfg(windows)]
fn schtasks(args: &[&str]) -> Result<String> {
    let out = schtasks_raw(args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        bail!("schtasks {}: {detail}", args.join(" "));
    }
    Ok(decode_windows_output(&out.stdout))
}

#[cfg(windows)]
fn decode_windows_output(bytes: &[u8]) -> String {
    // `schtasks /Query /XML` outputs UTF-16LE with BOM on many Windows builds.
    if bytes.starts_with(&[0xFF, 0xFE]) && bytes.len() >= 2 {
        let words: Vec<u16> = bytes[2..].as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
        return String::from_utf16_lossy(&words);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
fn query_xml() -> Option<String> {
    schtasks(&["/Query", "/TN", WINDOWS_TASK_NAME, "/XML"]).ok()
}

#[cfg(windows)]
pub fn installed() -> bool {
    query_xml().is_some_and(|xml| is_goatty_task(&xml))
}

#[cfg(windows)]
pub fn service_program() -> Option<PathBuf> {
    let xml = query_xml()?;
    is_goatty_task(&xml).then(|| task_program(&xml)).flatten()
}

#[cfg(windows)]
pub fn service_active() -> bool {
    let running_in_scheduler = schtasks(&["/Query", "/TN", WINDOWS_TASK_NAME, "/FO", "LIST"]).is_ok_and(|o| parse_schtasks_running(&o));
    if running_in_scheduler {
        return true;
    }
    // `schtasks /FO LIST` status strings are localized on non-English Windows;
    // also check whether our task's named pipe is actively served.
    installed() && unit_socket_path().is_some_and(|p| crate::ipc::is_listening(&p))
}

#[cfg(windows)]
pub fn start_socket() -> Result<()> {
    schtasks(&["/Run", "/TN", WINDOWS_TASK_NAME]).map(drop)
}

#[cfg(windows)]
pub fn stop_service() -> Result<()> {
    let _ = schtasks(&["/End", "/TN", WINDOWS_TASK_NAME]);
    Ok(())
}

#[cfg(windows)]
pub fn status() -> Status {
    let xml = query_xml();
    let is_ours = xml.as_deref().is_some_and(is_goatty_task);
    let exec_start = if is_ours { xml.as_deref().and_then(task_exec_display) } else { None };
    let active = is_ours && service_active();
    let pipe_listening = unit_socket_path().is_some_and(|p| crate::ipc::is_listening(&p));
    Status {
        unit_dir: unit_dir(),
        installed: is_ours,
        exec_start,
        socket_active: pipe_listening,
        service_active: active,
        linger: None,
        keeps_sessions: false,
        log_path: log_path(),
    }
}

#[cfg(windows)]
fn encode_utf16le_with_bom(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + text.len() * 2);
    out.extend_from_slice(&[0xFF, 0xFE]);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

#[cfg(windows)]
pub fn install(exe: &Path, socket_path: &Path) -> Result<Installed> {
    let unit_socket = unit_socket_path().context("couldn't determine default Windows pipe path")?;
    if socket_path != unit_socket {
        bail!(
            "goattyd uses {} (GOATTY_SOCKET or XDG_RUNTIME_DIR?), but the Windows Scheduled Task serves {}",
            socket_path.display(),
            unit_socket.display()
        );
    }

    if let Some(existing) = query_xml()
        && !is_goatty_task(&existing)
    {
        bail!("Scheduled Task \\{WINDOWS_TASK_NAME} already exists and wasn't written by goatty; remove it first");
    }

    let log = log_path().context("LOCALAPPDATA / USERPROFILE is not set")?;
    let dir = log.parent().context("log path has no parent")?.to_path_buf();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    let xml = render_task_xml(exe, &log, current_user_id().as_deref());
    let xml_path = dir.join(format!(".goattyd-task-{}.xml", std::process::id()));
    std::fs::write(&xml_path, encode_utf16le_with_bom(&xml)).with_context(|| format!("write {}", xml_path.display()))?;
    let xml_str = xml_path.to_string_lossy().into_owned();
    let created = schtasks(&["/Create", "/TN", WINDOWS_TASK_NAME, "/XML", &xml_str, "/F"]);
    let _ = std::fs::remove_file(&xml_path);
    created?;

    let foreign = crate::ipc::is_listening(socket_path) && !service_active();
    if !foreign {
        start_socket()?;
    }
    Ok(Installed { unit_dir: dir, started: !foreign, linger: None })
}

#[cfg(windows)]
pub fn uninstall() -> Result<bool> {
    let Some(existing) = query_xml() else {
        return Ok(false);
    };
    if !is_goatty_task(&existing) {
        bail!("Scheduled Task \\{WINDOWS_TASK_NAME} wasn't written by goatty; leaving it alone");
    }
    let _ = schtasks(&["/End", "/TN", WINDOWS_TASK_NAME]);
    schtasks(&["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_xml_is_well_formed_and_round_trips() {
        let exe = Path::new(r"C:\Program Files\goatty & co\goattyd.exe");
        let log = Path::new(r"C:\Users\alice\AppData\Local\goatty\goattyd.log");
        let xml = render_task_xml(exe, log, Some(r"DESKTOP\alice"));

        assert!(is_goatty_task(&xml));
        assert!(xml.contains(r#"<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">"#));

        // Verify top-level Task Scheduler 1.2 <xs:sequence> element order.
        let sections = ["<RegistrationInfo>", "<Triggers>", "<Principals>", "<Settings>", "<Actions Context=\"Author\">"];
        let positions: Vec<usize> = sections.iter().map(|s| xml.find(s).unwrap_or_else(|| panic!("missing {s}"))).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "top-level section order invalid: {positions:?}");

        let trigger = xml.split("<LogonTrigger>").nth(1).unwrap().split("</LogonTrigger>").next().unwrap();
        let enabled_pos = trigger.find("<Enabled>true</Enabled>").expect("LogonTrigger missing Enabled");
        let trigger_user_pos = trigger.find(r"<UserId>DESKTOP\alice</UserId>").expect("LogonTrigger missing UserId");
        assert!(enabled_pos < trigger_user_pos, "Task Scheduler schema requires <Enabled> before <UserId> in <LogonTrigger>: {trigger}");
        let principal = xml.split(r#"<Principal id="Author">"#).nth(1).unwrap().split("</Principal>").next().unwrap();
        let principal_user_pos = principal.find(r"<UserId>DESKTOP\alice</UserId>").expect("Principal missing UserId");
        let logon_type_pos = principal.find("<LogonType>InteractiveToken</LogonType>").expect("Principal missing LogonType");
        let run_level_pos = principal.find("<RunLevel>LeastPrivilege</RunLevel>").expect("Principal missing RunLevel");
        assert!(principal_user_pos < logon_type_pos && logon_type_pos < run_level_pos, "invalid Principal element order: {principal}");
        let exec = xml.split("<Exec>").nth(1).unwrap().split("</Exec>").next().unwrap();
        assert!(exec.find("<Command>").unwrap() < exec.find("<Arguments>").unwrap(), "invalid Exec element order: {exec}");
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains("<Hidden>true</Hidden>"));
        assert!(xml.contains("&amp;"), "escapes ampersands: {xml}");
        assert_eq!(task_program(&xml).unwrap(), exe);
        assert_eq!(
            task_exec_display(&xml).unwrap(),
            r#""C:\Program Files\goatty & co\goattyd.exe" serve --log C:\Users\alice\AppData\Local\goatty\goattyd.log"#
        );

        // Also verify rendering without an explicit UserId remains valid.
        let anon_xml = render_task_xml(exe, log, None);
        assert!(!anon_xml.contains("<UserId>"));
        assert!(anon_xml.contains("<LogonTrigger>\n      <Enabled>true</Enabled>\n    </LogonTrigger>"));
    }

    #[cfg(windows)]
    #[test]
    fn task_xml_validates_against_windows_task_scheduler_schema() {
        fn validate_with_task_scheduler(xml: &str) -> Result<()> {
            let dir = tempfile::tempdir().context("create tempdir")?;
            let path = dir.path().join("task.xml");
            std::fs::write(&path, encode_utf16le_with_bom(xml)).context("write temp task xml")?;
            let script = r#"
$ErrorActionPreference = 'Stop'
$xml = Get-Content -LiteralPath $env:GOATTY_TASK_XML -Raw -Encoding Unicode
$svc = New-Object -ComObject Schedule.Service
$svc.Connect()
$td = $svc.NewTask(0)
$td.XmlText = $xml
"#;
            let out = Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", script])
                .env("GOATTY_TASK_XML", &path)
                .stdin(Stdio::null())
                .output()
                .context("run powershell.exe")?;
            if !out.status.success() {
                bail!(
                    "Task Scheduler rejected XML:\nstdout: {}\nstderr: {}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Ok(())
        }

        let exe = Path::new(r"C:\Program Files\goatty & co\goattyd.exe");
        let log = Path::new(r"C:\Users\alice\AppData\Local\goatty\goattyd.log");

        // Both user-scoped and unscoped task definitions must pass Windows' real
        // Task Scheduler 1.2 XSD validator (`ITaskDefinition::put_XmlText`).
        let with_user = render_task_xml(exe, log, Some(r"DESKTOP\alice"));
        validate_with_task_scheduler(&with_user).expect("user-scoped task XML must pass Task Scheduler schema validation");

        let without_user = render_task_xml(exe, log, None);
        validate_with_task_scheduler(&without_user).expect("unscoped task XML must pass Task Scheduler schema validation");

        // Negative control: swapping <UserId> before <Enabled> in <LogonTrigger>
        // must be rejected by the schema validator.
        let invalid = with_user.replace(
            "<LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>DESKTOP\\alice</UserId>",
            "<LogonTrigger>\n      <UserId>DESKTOP\\alice</UserId>\n      <Enabled>true</Enabled>",
        );
        assert_ne!(invalid, with_user);
        assert!(
            validate_with_task_scheduler(&invalid).is_err(),
            "Task Scheduler schema validator must reject out-of-order LogonTrigger elements"
        );
    }

    #[test]
    fn parses_schtasks_list_status() {
        let running = "\nHostName:                             WINBOX\nTaskName:                             \\goattyd\nStatus:                               Running\n";
        let ready = "\nTaskName:                             \\goattyd\nStatus:                               Ready\n";
        assert!(parse_schtasks_running(running));
        assert!(!parse_schtasks_running(ready));
    }

    #[test]
    fn log_path_uses_localappdata() {
        let p = log_path_from(|k| (k == "LOCALAPPDATA").then(|| std::ffi::OsString::from(r"C:\Users\alice\AppData\Local"))).unwrap();
        assert_eq!(p, PathBuf::from(r"C:\Users\alice\AppData\Local").join("goatty").join("goattyd.log"));
    }
}
