//! aw-timer: Rize-style focus timer built on top of ActivityWatch.

mod categorize;
mod claude_code;
mod client;
mod config;
mod journal;
mod menubar;
mod notify;
mod report;
mod sessions;
mod sisyphus;
mod sound;
mod soundsynth;
mod tracker;

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::SystemTime;

use client::AwClient;

const LAUNCHD_LABEL: &str = "local.aw-timer";
const BREW_LABEL: &str = "homebrew.mxcl.aw-timer";

const USAGE: &str = "\
用法：aw-timer [--config 路徑] <指令>

指令：
  run [-v] [--headless]
                 背景計時器＋選單列（launchd 會執行這個）
                 -v 每次檢查都印出狀態；--headless 不顯示選單列
  status         目前狀態
  report [日期] [-i]
                 每日報表；日期可為 YYYY-MM-DD、today、yesterday 或 -N（N 天前）
                 -i 報表後可手動把項目分類成專注／娛樂／中性
  test-notify    送出一則測試通知
  config         顯示目前使用的設定檔路徑
  restart        重新啟動背景計時器以套用新設定
  preview-icons <out.png>  輸出選單列圖示的動畫格（除錯用）
  sound-check [音量]  逐一試播背景音，確認真的有音訊（預設音量 0；除錯用）";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Extract the global `--config PATH` / `--config=PATH` option.
    let mut config_path: Option<String> = None;
    if let Some(i) = args.iter().position(|a| a == "--config" || a.starts_with("--config=")) {
        let arg = args.remove(i);
        config_path = match arg.split_once('=') {
            Some((_, v)) => Some(v.to_owned()),
            None if i < args.len() => Some(args.remove(i)),
            None => {
                eprintln!("--config 需要一個路徑\n\n{USAGE}");
                return ExitCode::from(2);
            }
        };
    }
    let Some(cmd) = args.first().cloned() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest = &args[1..];

    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        "restart" => {
            // SAFETY: getuid has no preconditions and cannot fail.
            let uid = unsafe { libc_getuid() };
            // install.sh registers LAUNCHD_LABEL; `brew services` uses its own.
            let ok = [LAUNCHD_LABEL, BREW_LABEL].iter().any(|label| {
                Command::new("launchctl")
                    .args(["kickstart", "-k", &format!("gui/{uid}/{label}")])
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            });
            return if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE };
        }
        _ => {}
    }

    let (settings, mut categorizer, path) = match config::load(config_path.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("設定檔錯誤：{e}");
            return ExitCode::from(2);
        }
    };
    let mut client = AwClient::new(&settings.server_url);

    let result = match cmd.as_str() {
        "config" => {
            println!("{path}");
            Ok(())
        }
        "test-notify" => {
            notify::notify("aw-timer", "通知正常運作 ✅", &settings.notify_sound);
            Ok(())
        }
        "run" => {
            let verbose = rest.iter().any(|a| a == "-v" || a == "--verbose");
            let reload = reloader(config_path.clone());
            if settings.menubar && !rest.iter().any(|a| a == "--headless") {
                menubar::run(settings, client, categorizer, verbose, reload)
            }
            tracker::run_forever(&mut client, &mut categorizer, &settings, verbose, |_| {}, reload, None)
        }
        "sound-check" => {
            let volume = rest.first().and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0);
            return sound_check(&settings, volume);
        }
        "preview-icons" => {
            let out = rest.first().map_or("sisyphus-preview.png", String::as_str);
            return match menubar::preview(out, 6.0) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            };
        }
        "status" => tracker::snapshot(&mut client, &mut categorizer, &mut claude_code::CodeIndex::default(), &settings)
            .map(|snap| println!("{}", snap.status_line())),
        "report" => {
            let interactive = rest.iter().any(|a| a == "-i" || a == "--classify");
            let day_arg = rest.iter().find(|a| *a != "-i" && *a != "--classify");
            let day = match report::parse_day(day_arg.map(String::as_str)) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            };
            let (start, end) = tracker::day_bounds(day);
            let color = std::io::stdout().is_terminal();
            let mut code = claude_code::CodeIndex::default();
            loop {
                let (acts, rests, entries) = match tracker::load_day(&mut client, &mut categorizer, &mut code, start, end) {
                    Ok(v) => v,
                    Err(e) => break Err(e),
                };
                println!("{}", report::render(&acts, day, &settings, color, &rests, &entries));
                let cands = report::candidates(&acts, 12);
                if !interactive || cands.is_empty() {
                    break Ok(());
                }
                print!("\n{}> ", report::render_candidates(&cands, color));
                let _ = std::io::stdout().flush();
                let mut input = String::new();
                if std::io::stdin().lock().read_line(&mut input).unwrap_or(0) == 0 || input.trim().is_empty() {
                    break Ok(());
                }
                match report::parse_choices(&input, &cands)
                    .and_then(|ov| config::save_overrides(&ov).map(|()| ov.len()))
                    .and_then(|n| config::load(config_path.as_deref()).map(|(_, c, _)| (n, c)))
                {
                    Ok((n, fresh)) => {
                        categorizer = fresh;
                        println!("\n已儲存 {n} 筆分類到 {}，重新產生報表…\n", config::overrides_path().display());
                    }
                    Err(e) => eprintln!("{e}"),
                }
            }
        }
        other => {
            eprintln!("未知指令：{other}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("連不到 ActivityWatch（{e}）。請確認它有在執行。");
            ExitCode::FAILURE
        }
    }
}

/// Play every in-process sound briefly at `volume` and report whether audio
/// is actually produced: radio = decoded chunks queued, loop = rendering.
fn sound_check(settings: &config::Settings, volume: f32) -> ExitCode {
    let mut player = sound::Player::new(settings.sound.clone()).with_test_gain(volume);
    let mut ok = true;
    for c in player.choices().to_vec() {
        if matches!(c.source, sound::Source::Spotify(_) | sound::Source::Feishin) {
            continue;
        }
        let started = std::time::Instant::now();
        let result = player.play(&c.source).and_then(|()| {
            for _ in 0..100 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let good = match c.source {
                    sound::Source::Stream(_) => player.queued_chunks() >= 2,
                    _ => player.loop_active(),
                };
                if good {
                    return Ok(());
                }
            }
            Err("10 秒內沒有音訊".to_owned())
        });
        let mark = if result.is_ok() { "✓" } else { ok = false; "✗" };
        println!("{mark} {:<36} {:.1}s {}", c.title, started.elapsed().as_secs_f64(), result.err().unwrap_or_default());
        player.stop();
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn mtimes(files: &[PathBuf]) -> Vec<Option<SystemTime>> {
    files.iter().map(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok()).collect()
}

/// Closure for the timer loop: a fresh categorizer whenever `config.toml`
/// or `overrides.toml` changed on disk since the last call.
fn reloader(explicit: Option<String>) -> impl FnMut() -> Option<categorize::Categorizer> + Send + 'static {
    let mut last = mtimes(&config::watched_files(explicit.as_deref()));
    move || {
        let now = mtimes(&config::watched_files(explicit.as_deref()));
        if now == last {
            return None;
        }
        last = now;
        match config::load(explicit.as_deref()) {
            Ok((_, c, _)) => Some(c),
            Err(e) => {
                eprintln!("設定檔錯誤（沿用舊設定）：{e}");
                None
            }
        }
    }
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}
