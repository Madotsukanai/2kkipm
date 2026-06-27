use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use colored::*;
use dirs::{config_dir, data_dir};
use encoding_rs::SHIFT_JIS;
use indicatif::{ProgressBar, ProgressStyle};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const WIKI_URL: &str = "https://wikiwiki.jp/yumenikki-g3/";
const WIKI_UPDATES_URL: &str =
    "https://wikiwiki.jp/yumenikki-g3/FrontPage/\
     %E6%9C%80%E8%BF%91%E3%81%AE%E4%BA%88%E5%AE%9A%E3%83%BB%E6%9B%B4%E6%96%B0%E4%B8%80%E8%A6%A7";
const STATE_FILE: &str = "y2k-pm/state.json";
const CONFIG_FILE: &str = "y2k-pm/config.json";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// ユーザーがCtrl+Cを押したかグローバルに追跡するフラグ
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

// ============================================================
// CLI
// ============================================================

#[derive(Parser)]
#[clap(
    name = "y2k",
    about = "ゆめ2っき パッケージマネージャー",
    version = "0.1.0"
)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Wikiからパッケージリストを更新し、最近の更新一覧を表示する
    Update {
        /// 表示件数 (省略時: 最新core〜最新patch間の全件)
        #[clap(short, long)]
        count: Option<usize>,
    },
    /// 最新バージョン情報を表示する
    Upgrade {
        #[clap(short, long)]
        download: bool,
    },
    /// coreまたはpatchをインストールする
    Install {
        kind: String,
    },
    /// インストール履歴を表示する
    List,
    /// 最近の更新一覧を表示する
    Show {
        /// 表示件数 (省略時: 最新core〜最新patch間の全件)
        #[clap(short, long)]
        count: Option<usize>,
    },
    /// 設定を表示・編集する
    Config {
        #[clap(long)]
        install_dir: Option<String>,
    },
    /// 状態ファイルをリセットする
    Clean,
}

// ============================================================
// 設定
// ============================================================

#[derive(Serialize, Deserialize, Debug, Default)]
struct Config {
    install_dir: Option<String>,
}

fn config_path() -> PathBuf {
    config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_FILE)
}

fn load_config() -> Config {
    let path = config_path();
    if path.exists() {
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        Config::default()
    }
}

fn save_config(config: &Config) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_string_pretty(config)?)?;
    Ok(())
}

fn prompt_install_dir(config: &mut Config) -> Result<PathBuf> {
    let stdin = io::stdin();
    loop {
        print!(
            "{} インストール先ディレクトリを入力してください: ",
            "?".cyan().bold()
        );
        io::stdout().flush()?;

        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let input = line.trim().to_string();

        if input.is_empty() {
            eprintln!("{} パスが空です。", "✗".red().bold());
            continue;
        }

        let expanded = expand_path(&input);
        let expanded = expanded.canonicalize().unwrap_or(expanded);
        config.install_dir = Some(expanded.to_string_lossy().to_string());
        save_config(config)?;
        println!(
            "{} install_dir を設定しました: {}",
            "✓".green().bold(),
            expanded.display().to_string().yellow()
        );
        return Ok(expanded);
    }
}

fn expand_path(input: &str) -> PathBuf {
    let p = if input.starts_with('~') {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        if input == "~" || input == "~/" {
            home
        } else if input.starts_with("~/") {
            home.join(&input[2..])
        } else {
            home.join(&input[1..])
        }
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(input)
    };
    p.canonicalize().unwrap_or(p)
}

// ============================================================
// 状態
// ============================================================

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
enum EntryKind {
    Core,
    Patch,
    Runner,
    Other,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct UpdateEntry {
    date: String,
    kind: EntryKind,
    label: String,
    authors: Vec<String>,
    note: Option<String>,
    dl_url: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
struct InstalledVersionState {
    installed_patch: Option<String>,
    install_history: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct State {
    last_fetched: Option<String>,
    entries: Vec<UpdateEntry>,
    #[serde(default)]
    installed_versions: std::collections::HashMap<String, InstalledVersionState>,
    active_core: Option<String>,
    
    // 後方互換性のためのフィールド
    installed_core: Option<String>,
    installed_patch: Option<String>,
    install_history: Vec<String>,
}

fn state_path() -> PathBuf {
    data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(STATE_FILE)
}

fn load_state() -> State {
    let path = state_path();
    if path.exists() {
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        State::default()
    }
}

fn save_state(state: &State) -> Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

// ============================================================
// 静的正規表現
// ============================================================

macro_rules! static_re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| Regex::new($pat).unwrap())
        }
    };
}

static_re!(tag_re,      r"<[^>]+>");
static_re!(entity_re,   r"&[a-z]+;|&#\d+;");
static_re!(spaces_re,   r"[\s\u{3000}]+");
static_re!(link_re,     r#"(?i)<a[^>]+href="([^"]+)"[^>]*>(.*?)</a>"#);
static_re!(auth_re,     r"([^\s/\(（\[【]+)氏");
static_re!(date_re,     r"^(\d{4}/\d{2}/\d{2})\s+(.+)$");
static_re!(label_re,    r"^(.+?)\s+DL");
static_re!(gdrive_id_re, r"(?:drive\.google\.com/(?:file/d/|open\?id=|uc\?.*?id=)|docs\.google\.com/\S+/d/)([a-zA-Z0-9_-]{25,})");
static_re!(form_action_re,   r#"(?i)<form\s[^>]*?action="([^"]+)""#);
static_re!(cd_utf8_re,  r"filename\*=UTF-8''([^;\r\n]+)");
static_re!(cd_ascii_re, r#"filename="([^"]+)""#);
static_re!(og_title_prop_first_re, r#"(?i)<meta[^>]+property="og:title"[^>]+content="([^"]+)""#);
static_re!(og_title_content_first_re, r#"(?i)<meta[^>]+content="([^"]+)"[^>]+property="og:title""#);
static_re!(title_tag_re, r"(?i)<title[^>]*>([^<]+)</title>");
static_re!(uc_name_size_re, r#"(?i)<span[^>]+class="uc-name-size"[^>]*>\s*<a[^>]*>(.*?)</a>"#);

static_re!(input_tag_re, r"(?i)<input\s+([^>]+)>");
static_re!(attr_name_re, r#"(?i)name=["']([^"']+)["']"#);
static_re!(attr_value_re, r#"(?i)value=["']([^"']*)["']"#);

// ============================================================
// HTML パース
// ============================================================

fn strip_tags(html: &str) -> String {
    let text = tag_re().replace_all(html, "");
    let text = entity_re().replace_all(&text, " ");
    spaces_re().replace_all(&text, " ").trim().to_string()
}

fn extract_links(html: &str) -> Vec<(String, String)> {
    link_re()
        .captures_iter(html)
        .map(|cap| {
            let url = cap[1].to_string();
            let text = tag_re().replace_all(&cap[2], "").to_string();
            (url, text)
        })
        .collect()
}

fn extract_dl_link_from_line(html_line: &str) -> Option<String> {
    extract_links(html_line).into_iter().find_map(|(url, _)| {
        let resolved = resolve_cushion_url(&url);
        let is_dl = resolved.contains("getuploader")
            || resolved.contains("firestorage")
            || resolved.contains("axfc")
            || resolved.contains("drive.google")
            || resolved.contains("dropbox")
            || resolved.contains("ux.getuploader");
        if is_dl { Some(resolved) } else { None }
    })
}

fn resolve_cushion_url(url: &str) -> String {
    if url.contains("wikiwiki.jp/p/cushion") {
        if let Some(to) = url.split("?to=").nth(1) {
            return percent_decode(to);
        }
    }
    url.to_string()
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes().peekable();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let h1 = bytes.next().unwrap_or(b'0') as char;
            let h2 = bytes.next().unwrap_or(b'0') as char;
            if let Ok(byte) = u8::from_str_radix(&format!("{h1}{h2}"), 16) {
                out.push(byte as char);
                continue;
            }
        }
        out.push(b as char);
    }
    out
}

fn extract_authors(text: &str) -> Vec<String> {
    auth_re()
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_entry(html_line: &str) -> Option<UpdateEntry> {
    let dl_url = extract_dl_link_from_line(html_line);
    let line = strip_tags(html_line);

    if line.is_empty() || line.starts_with('※') {
        return None;
    }
    let caps = date_re().captures(&line)?;
    let date = caps[1].to_string();
    let rest = caps[2].trim().to_string();

    if rest.starts_with('※') {
        return None;
    }
    if rest.contains("走者予約") || rest.contains("走っています") {
        return Some(UpdateEntry {
            date,
            kind: EntryKind::Runner,
            label: rest.clone(),
            authors: extract_authors(&rest),
            note: None,
            dl_url: None,
        });
    }
    if !rest.contains("DL") {
        return None;
    }

    let label = label_re()
        .captures(&rest)
        .map(|c| c[1].trim().to_string())
        .unwrap_or_else(|| rest.clone());

    let kind = if label.starts_with("ver") && !label.contains("パッチ") {
        EntryKind::Core
    } else {
        EntryKind::Patch
    };

    let note = if let Some(pos) = rest.find("DL") {
        let after_dl = rest[pos + 2..].trim();
        if !after_dl.is_empty() {
            Some(after_dl.to_string())
        } else {
            None
        }
    } else {
        None
    };

    Some(UpdateEntry {
        date,
        kind,
        label,
        authors: extract_authors(&rest),
        note,
        dl_url,
    })
}

fn fetch_updates() -> Result<Vec<UpdateEntry>> {
    eprint!("{}", "  Wikiを取得中...".dimmed());

    let response = ureq::get(WIKI_UPDATES_URL)
        .set("User-Agent", USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .call();

    let response = match response {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => {
            eprintln!();
            bail!(
                "Wiki がステータス {} を返しました。\n\
                 ブラウザで直接確認してください: {}",
                code, WIKI_UPDATES_URL
            );
        }
        Err(ureq::Error::Transport(t)) => {
            eprintln!();
            bail!("Wiki への接続に失敗しました。\n原因: {}", t);
        }
    };

    let status = response.status();
    if status != 200 {
        eprintln!(" {}", format!("警告: HTTP {}", status).yellow());
    } else {
        eprintln!(" {}", "完了".green());
    }

    let html = response.into_string()?;
    let mut entries: Vec<UpdateEntry> = Vec::new();
    let mut pending_note: Option<String> = None;
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    for html_line in html.lines() {
        let text = strip_tags(html_line);
        if text.is_empty() {
            continue;
        }
        if text.starts_with('※') {
            pending_note = Some(text);
            continue;
        }
        if !text.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        if let Some(mut entry) = parse_entry(html_line) {
            let key = (entry.date.clone(), entry.label.clone());
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            if let Some(p_note) = pending_note.take() {
                if let Some(ref mut n) = entry.note {
                    *n = format!("{} / {}", p_note, n);
                } else {
                    entry.note = Some(p_note);
                }
            }
            entries.push(entry);
        }
    }

    // メインページから過去ログのリンクを抽出する
    // 例: href="/yumenikki-g3/FrontPage/%E6%9C%80%E8%BF%91%E3%81%AE%E4%BA%88%E5%AE%9A%E3%83%BB%E6%9B%B4%E6%96%B0%E4%B8%80%E8%A6%A7/%E9%81%8E%E5%8E%BB%E3%83%AD%E3%82%B02015"
    let archive_re = Regex::new(
        r#"href="/yumenikki-g3/FrontPage/%E6%9C%80%E8%BF%91%E3%81%AE%E4%BA%88%E5%AE%9A%E3%83%BB%E6%9B%B4%E6%96%B0%E4%B8%80%E8%A6%A7/%E9%81%8E%E5%8E%BB%E3%83%AD%E3%82%B0(\d{4})""#
    ).unwrap();

    let mut archive_years = Vec::new();
    for cap in archive_re.captures_iter(&html) {
        if let Ok(year) = cap[1].parse::<u32>() {
            archive_years.push(year);
        }
    }
    archive_years.sort_by(|a, b| b.cmp(a)); // 新しい年（2025, 2024...）の順に処理する
    archive_years.dedup();

    for year in archive_years {
        let archive_url = format!(
            "https://wikiwiki.jp/yumenikki-g3/FrontPage/%E6%9C%80%E8%BF%91%E3%81%AE%E4%BA%88%E5%AE%9A%E3%83%BB%E6%9B%B4%E6%96%B0%E4%B8%80%E8%A6%A7/%E9%81%8E%E5%8E%BB%E3%83%AD%E3%82%B0{}",
            year
        );
        eprint!("{}", format!("  過去ログ{}を取得中...", year).dimmed());
        
        let res = ureq::get(&archive_url)
            .set("User-Agent", USER_AGENT)
            .timeout(std::time::Duration::from_secs(30))
            .call();

        let html_archive = match res {
            Ok(r) => {
                eprintln!(" {}", "完了".green());
                r.into_string().unwrap_or_default()
            }
            Err(e) => {
                eprintln!(" {}", format!("失敗 (スキップ): {}", e).yellow());
                continue;
            }
        };

        let mut archive_pending_note: Option<String> = None;
        for html_line in html_archive.lines() {
            let text = strip_tags(html_line);
            if text.is_empty() {
                continue;
            }
            if text.starts_with('※') {
                archive_pending_note = Some(text);
                continue;
            }
            if !text.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            if let Some(mut entry) = parse_entry(html_line) {
                let key = (entry.date.clone(), entry.label.clone());
                if seen.contains(&key) {
                    continue;
                }
                seen.insert(key);
                if let Some(p_note) = archive_pending_note.take() {
                    if let Some(ref mut n) = entry.note {
                        *n = format!("{} / {}", p_note, n);
                    } else {
                        entry.note = Some(p_note);
                    }
                }
                entries.push(entry);
            }
        }
    }

    // 日付の降順（新しい日付が先）で Stable Sort を行う。
    entries.sort_by(|a, b| b.date.cmp(&a.date));

    Ok(entries)
}

// ============================================================
// 表示範囲の計算
// ============================================================

fn default_display_count(entries: &[UpdateEntry]) -> usize {
    let core_pos  = entries.iter().position(|e| e.kind == EntryKind::Core);
    let patch_pos = entries.iter().position(|e| e.kind == EntryKind::Patch);
    match (core_pos, patch_pos) {
        (Some(c), Some(p)) => c.max(p) + 1,
        (Some(c), None)    => c + 1,
        (None, Some(p))    => p + 1,
        (None, None)       => entries.len(),
    }
}

// ============================================================
// Helper: x-www-form-urlencoded 構築
// ============================================================

fn form_urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ============================================================
// getuploader ダウンロード
// ============================================================

fn getuploader_download(url: &str, hint_dest: Option<&PathBuf>) -> Result<PathBuf> {
    let agent = ureq::AgentBuilder::new().redirects(0).build(); // リダイレクトを追わない
    eprintln!("  {} getuploader ページを解析中...", "→".cyan());

    // 1. GETリクエスト
    let res = agent.get(url).set("User-Agent", USER_AGENT).call()?;
    let html = res.into_string()?;

    // 2. 隠しパラメータ(token)の抽出
    let mut token = String::new();
    for cap in input_tag_re().captures_iter(&html) {
        let attrs = &cap[1];
        if attrs.contains("name=\"token\"") || attrs.contains("name='token'") {
            if let Some(v) = attr_value_re().captures(attrs) {
                token = v[1].to_string();
                break;
            }
        }
    }

    if token.is_empty() {
        bail!("トークンが見つかりませんでした。サイト構造が変わった可能性があります。");
    }

    // 3. トークンをPOST送信
    eprintln!("  {} トークンをPOST送信中...", "→".cyan());
    let res2 = agent.post(url)
        .set("User-Agent", USER_AGENT)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&format!("token={}", token))
        .map_err(|e| anyhow::anyhow!("POST失敗: {}", e))?;

    let body = res2.into_string()?;

    // 4. HTMLボディから実ファイルのダウンロードURLを取得
    let refresh_re = Regex::new(r#"(?i)<meta\s+http-equiv=["']refresh["']\s+content=["']\d+;\s*URL=([^"']+)["']"#)?;
    let dl_now_re = Regex::new(r#"(?i)<a\s+[^>]*href="([^"]+)"[^>]*>Download\s+Now</a>"#)?;

    let mut raw_dl_url = None;
    if let Some(cap) = refresh_re.captures(&body) {
        raw_dl_url = Some(cap[1].to_string());
    } else if let Some(cap) = dl_now_re.captures(&body) {
        raw_dl_url = Some(cap[1].to_string());
    }

    let raw_dl_url = raw_dl_url.context("実ファイルのダウンロードURLが見つかりませんでした。サイト構造が変わった可能性があります。")?;
    let dl_url = decode_html_entities(&raw_dl_url);
    eprintln!("  {} 実ファイルURLを取得: {}", "✓".green(), dl_url.yellow());

    // 5. 実ファイルをダウンロード
    let res3 = agent.get(&dl_url).set("User-Agent", USER_AGENT).call()?;
    
    let fname = filename_from_response(&res3).or_else(|| {
        url::Url::parse(&dl_url).ok()
            .and_then(|u| u.path_segments()?.last().map(|s| s.to_string()))
    });

    let dest = resolve_dest(hint_dest, fname.as_deref(), "y2k-patch.zip");
    stream_to_file(res3, &dest)?;

    Ok(dest)
}

fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
     .replace("&#45;", "-")
     .replace("&#045;", "-")
     .replace("&#39;", "'")
     .replace("&quot;", "\"")
     .replace("&lt;", "<")
     .replace("&gt;", ">")
}

// ============================================================
// Google Drive ダウンロード
// ============================================================

fn extract_gdrive_id(url: &str) -> Option<String> {
    gdrive_id_re().captures(url).map(|c| c[1].to_string())
}

fn build_viruscheck_url(html: &str, file_id: &str) -> Option<String> {
    let mut id_val      = file_id.to_string();
    let mut authuser    = "0".to_string();
    let mut confirm_val = "t".to_string();
    let mut uuid_val    = String::new();

    for cap in input_tag_re().captures_iter(html) {
        let attrs = &cap[1];
        let attrs_lower = attrs.to_lowercase();
        if attrs_lower.contains("type=\"hidden\"") || attrs_lower.contains("type='hidden'") || attrs_lower.contains("type=hidden") {
            let name = if let Some(n) = attr_name_re().captures(attrs) { n[1].to_lowercase() } else { continue; };
            let value = if let Some(v) = attr_value_re().captures(attrs) { v[1].to_string() } else { "".to_string() };
            match name.as_str() {
                "id"       => id_val      = value,
                "authuser" => authuser    = value,
                "confirm"  => confirm_val = value,
                "uuid"     => uuid_val    = value,
                _          => {}
            }
        }
    }

    if uuid_val.is_empty() {
        return None;
    }

    Some(format!(
        "https://drive.usercontent.google.com/download\
         ?id={}&authuser={}&confirm={}&uuid={}",
        id_val, authuser, confirm_val, uuid_val
    ))
}

fn get_url_from_gdrive_form(html: &str) -> Option<String> {
    let action_caps = form_action_re().captures(html)?;
    let action = action_caps[1]
        .replace("&amp;", "&")
        .replace("&#39;", "'");
    if action.is_empty() {
        return None;
    }
    let base = if action.starts_with("http") {
        action.clone()
    } else if action.starts_with('/') {
        format!("https://drive.google.com{}", action)
    } else {
        action.clone()
    };

    let mut params: Vec<(String, String)> = Vec::new();
    for cap in input_tag_re().captures_iter(html) {
        let attrs = &cap[1];
        let attrs_lower = attrs.to_lowercase();
        if attrs_lower.contains("type=\"hidden\"") || attrs_lower.contains("type='hidden'") || attrs_lower.contains("type=hidden") {
            let name = if let Some(n) = attr_name_re().captures(attrs) { n[1].to_string() } else { continue; };
            let value = if let Some(v) = attr_value_re().captures(attrs) { v[1].to_string() } else { "".to_string() };
            params.push((name, value.replace("&amp;", "&")));
        }
    }

    if params.is_empty() {
        return Some(base);
    }
    let sep = if base.contains('?') { "&" } else { "?" };
    let qs  = params.iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");
    Some(format!("{}{}{}", base, sep, qs))
}

fn looks_like_filename(s: &str) -> bool {
    const KNOWN_EXTS: &[&str] = &[
        ".zip", ".7z", ".rar", ".tar.gz", ".tgz", ".tar.xz", ".txz",
        ".tar.bz2", ".tbz2", ".exe", ".bin", ".lzh",
    ];
    let lower = s.to_lowercase();
    KNOWN_EXTS.iter().any(|ext| lower.ends_with(ext))
}

fn decode_basic_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn gdrive_filename_from_confirm_page(html: &str) -> Option<String> {
    if let Some(caps) = uc_name_size_re().captures(html) {
        let inner = strip_tags(&caps[1]);
        let name = decode_basic_entities(inner.trim());
        if !name.is_empty() {
            return Some(name);
        }
    }

    let from_og = og_title_prop_first_re()
        .captures(html)
        .or_else(|| og_title_content_first_re().captures(html))
        .map(|c| c[1].trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(name) = from_og {
        if looks_like_filename(&name) {
            return Some(name);
        }
        return None;
    }

    title_tag_re()
        .captures(html)
        .map(|c| {
            let t = c[1].trim();
            t.split(" - Google").next().unwrap_or(t).trim().to_string()
        })
        .filter(|s| {
            !s.is_empty()
                && s != "Google Drive"
                && s != "Google ドライブ"
                && looks_like_filename(s)
        })
}

fn urlencoding_decode(s: &str) -> String {
    percent_decode(&s.replace('+', " "))
}

fn filename_from_response(res: &ureq::Response) -> Option<String> {
    let cd = res.header("Content-Disposition")?;
    let decoded = urlencoding_decode(cd);
    if let Some(caps) = cd_utf8_re().captures(&decoded) {
        return Some(urlencoding_decode(caps[1].trim()));
    }
    if let Some(caps) = cd_ascii_re().captures(&decoded) {
        return Some(caps[1].to_string());
    }
    None
}

fn gdrive_download(file_id: &str, hint_dest: Option<&PathBuf>) -> Result<PathBuf> {
    let agent = ureq::AgentBuilder::new().redirects(10).build();
    let initial_url = format!(
        "https://drive.google.com/uc?export=download&id={}", file_id
    );

    eprintln!("  {} Google Drive から取得中...", "→".cyan());

    let res = agent
        .get(&initial_url)
        .set("User-Agent", USER_AGENT)
        .timeout(std::time::Duration::from_secs(60))
        .call();

    let res = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => bail!(
            "Google Drive がステータス {} を返しました。\n{}",
            code,
            r.into_string().unwrap_or_default().chars().take(300).collect::<String>()
        ),
        Err(e) => bail!("Google Drive への接続に失敗: {}", e),
    };

    let content_type = res.header("Content-Type").unwrap_or("").to_string();

    if !content_type.contains("text/html") {
        let fname = filename_from_response(&res);
        let dest  = resolve_dest(hint_dest, fname.as_deref(), "y2k-download.bin");
        eprintln!("  {} 直接ダウンロード開始", "✓".green());
        stream_to_file(res, &dest)?;
        return Ok(dest);
    }

    let html = res.into_string()?;

    if html.contains("Quota exceeded") || html.contains("can't view or download") {
        bail!(
            "Google Drive の共有ダウンロード上限を超えています。\n\
             ブラウザで直接確認: https://drive.google.com/file/d/{}/view",
            file_id
        );
    }

    let og_fname = gdrive_filename_from_confirm_page(&html);

    if let Some(ref f) = og_fname {
        eprintln!("  {} ファイル名(ページより): {}", "→".cyan(), f.yellow());
    }

    let dl_url = if let Some(url) = build_viruscheck_url(&html, file_id) {
        eprintln!("  {} ウイルスチェックページを通過中...", "→".cyan());
        url
    } else if let Some(url) = get_url_from_gdrive_form(&html) {
        eprintln!("  {} 確認ページを通過中...", "→".cyan());
        url
    } else {
        bail!(
            "確認ページから DL URL を取得できませんでした。\n\
             ブラウザで直接確認: https://drive.google.com/file/d/{}/view\n\
             --- ページ冒頭 ---\n{}",
            file_id,
            html.chars().take(500).collect::<String>()
        )
    };

    let res2 = agent
        .get(&dl_url)
        .set("User-Agent", USER_AGENT)
        .timeout(std::time::Duration::from_secs(3600))
        .call();

    let res2 = match res2 {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => bail!(
            "ステータス {} が返されました。\n{}",
            code,
            r.into_string().unwrap_or_default().chars().take(300).collect::<String>()
        ),
        Err(e) => bail!("ダウンロードリクエストに失敗: {}", e),
    };

    let ct2 = res2.header("Content-Type").unwrap_or("").to_string();
    if ct2.contains("text/html") {
        let body = res2.into_string().unwrap_or_default();
        if body.contains("Quota exceeded") || body.contains("can't view or download") {
            bail!(
                "Google Drive のダウンロード制限（クォータ上限）に達しています。\n\
                 しばらく時間を置くか、ブラウザで以下のURLを開き、右上のオプションから「ドライブにショートカットを追加」または「コピーを作成」を行った上で、ご自身のマイドライブからダウンロードしてください。\n\
                 ブラウザで確認: https://drive.google.com/file/d/{}/view",
                file_id
            );
        }
        bail!(
            "ダウンロード URL が再度 HTML を返しました。\n\
             ブラウザで直接確認: https://drive.google.com/file/d/{}/view\n\
             --- ページ冒頭 ---\n{}",
            file_id,
            body.chars().take(400).collect::<String>()
        );
    }

    let fname = filename_from_response(&res2).or(og_fname);
    if let Some(ref f) = fname {
        eprintln!("  {} ファイル名: {}", "→".cyan(), f.yellow());
    }
    let dest = resolve_dest(hint_dest, fname.as_deref(), "y2k-download.bin");
    stream_to_file(res2, &dest)?;
    Ok(dest)
}

// ============================================================
// 汎用ダウンロード
// ============================================================

fn download_file(url: &str, hint_dest: Option<&PathBuf>) -> Result<PathBuf> {
    if url.contains("getuploader.com") {
        return getuploader_download(url, hint_dest);
    }
    if let Some(file_id) = extract_gdrive_id(url) {
        return gdrive_download(&file_id, hint_dest);
    }
    let agent = ureq::AgentBuilder::new().redirects(10).build();
    let res = agent
        .get(url)
        .set("User-Agent", USER_AGENT)
        .timeout(std::time::Duration::from_secs(3600))
        .call()
        .with_context(|| format!("ダウンロードに失敗しました: {}", url))?;
    let dest = resolve_dest(hint_dest, filename_from_response(&res).as_deref(), "download.bin");
    stream_to_file(res, &dest)?;
    Ok(dest)
}

fn resolve_dest(hint_dest: Option<&PathBuf>, fname: Option<&str>, fallback: &str) -> PathBuf {
    let name = fname.unwrap_or(fallback);
    if let Some(p) = hint_dest {
        if fname.is_some() {
            if let Some(parent) = p.parent() {
                return parent.join(name);
            }
        }
        return p.clone();
    }
    std::env::temp_dir().join(name)
}

fn stream_to_file(response: ureq::Response, dest: &PathBuf) -> Result<()> {
    let total: Option<u64> = response
        .header("Content-Length")
        .and_then(|v| v.parse().ok());
    let pb = if let Some(t) = total {
        ProgressBar::new(t)
    } else {
        ProgressBar::new_spinner()
    };
    let style = ProgressStyle::with_template(
        "  {spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] \
         {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("█▓░");
    pb.set_style(style);

    const BUF: usize = 1024 * 1024; // 1 MB

    struct ProgressReader<R: io::Read> {
        inner: R,
        pb: ProgressBar,
    }
    impl<R: io::Read> io::Read for ProgressReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.pb.inc(n as u64);
            Ok(n)
        }
    }

    let raw_reader = response.into_reader();
    let buf_reader = io::BufReader::with_capacity(BUF, raw_reader);
    let progress_reader = ProgressReader { inner: buf_reader, pb: pb.clone() };

    let file = fs::File::create(dest)?;
    let mut buf_writer = io::BufWriter::with_capacity(BUF, file);

    io::copy(&mut { progress_reader }, &mut buf_writer)
        .context("ダウンロード中にエラーが発生しました")?;

    buf_writer.flush().context("ファイルの書き込みを完了できませんでした")?;
    pb.finish_and_clear();
    Ok(())
}

// ============================================================
// アーカイブ展開
// ============================================================

fn list_top_level_entries(dir: &PathBuf) -> std::collections::HashSet<PathBuf> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}

fn diff_new_top_level_entries(
    dir: &PathBuf,
    before: &std::collections::HashSet<PathBuf>,
) -> Vec<PathBuf> {
    list_top_level_entries(dir)
        .into_iter()
        .filter(|p| !before.contains(p))
        .collect()
}

fn remove_new_top_level_entries(dir: &PathBuf, before: &std::collections::HashSet<PathBuf>) {
    for p in diff_new_top_level_entries(dir, before) {
        if p.is_dir() {
            let _ = fs::remove_dir_all(&p);
        } else {
            let _ = fs::remove_file(&p);
        }
    }
}

fn make_extract_pb(total: u64, unit: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    let style = if unit == "files" {
        ProgressStyle::with_template(
            "  {spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ファイル"
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("█▓░")
    } else {
        ProgressStyle::with_template(
            "  {spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] \
             {bytes}/{total_bytes} ({bytes_per_sec})"
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("█▓░")
    };
    pb.set_style(style);
    pb
}

fn find_active_core_dir(install_dir: &PathBuf, active_core: Option<&str>) -> PathBuf {
    if let Some(core) = active_core {
        let core_num = core.trim_start_matches("ver");
        
        let path = install_dir.join(format!("ゆめ2っきver{}", core_num));
        if path.exists() && path.is_dir() {
            return path;
        }

        let path = install_dir.join(format!("ゆめ2っき{}", core_num));
        if path.exists() && path.is_dir() {
            return path;
        }

        if let Ok(rd) = fs::read_dir(install_dir) {
            for entry in rd {
                if let Ok(entry) = entry {
                    let path = entry.path();
                    if path.is_dir() {
                        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                        if name.starts_with("ゆめ2っき") && name.contains(core_num) {
                            return path;
                        }
                    }
                }
            }
        }
    }

    if let Ok(rd) = fs::read_dir(install_dir) {
        for entry in rd {
            if let Ok(entry) = entry {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    if name.starts_with("ゆめ2っきver") {
                        return path;
                    }
                }
            }
        }
    }
    install_dir.clone()
}

fn find_patch_real_root(dir: &PathBuf) -> PathBuf {
    if let Ok(rd) = fs::read_dir(dir) {
        let mut has_game_indicator = false;
        let mut subdirs = Vec::new();
        
        for entry in rd {
            if let Ok(entry) = entry {
                let name = entry.file_name().to_string_lossy().to_string();
                let name_lower = name.to_lowercase();
                if name == "ゆめ2っき" || name_lower == "yume2kki" || name_lower.starts_with("rpg_rt") {
                    has_game_indicator = true;
                    break;
                }
                if entry.path().is_dir() {
                    subdirs.push(entry.path());
                }
            }
        }
        
        if has_game_indicator {
            return dir.clone();
        }
        
        if subdirs.len() == 1 {
            return find_patch_real_root(&subdirs[0]);
        }
    }
    dir.clone()
}

fn try_promote_version(state: &mut State, install_dir: &PathBuf, old_ver: &str, patch_label: &str) -> Result<()> {
    let re = regex::Regex::new(r"ver\d+\.\d+[a-z]?").unwrap();
    let new_ver = if let Some(cap) = re.captures(patch_label) {
        cap[0].to_string()
    } else {
        return Ok(());
    };

    if new_ver == old_ver {
        return Ok(());
    }

    println!("  {} バージョン昇格を検出: {} -> {}", "→".cyan(), old_ver.yellow(), new_ver.green());

    let old_num = old_ver.trim_start_matches("ver");
    let new_num = new_ver.trim_start_matches("ver");

    let old_dir = install_dir.join(format!("ゆめ2っきver{}", old_num));
    let new_dir = install_dir.join(format!("ゆめ2っきver{}", new_num));

    if old_dir.exists() && old_dir.is_dir() {
        eprintln!("  {} ディレクトリをリネーム中: {} -> {}", "→".cyan(), old_dir.display(), new_dir.display());
        fs::rename(&old_dir, &new_dir)?;
    } else {
        let old_dir_alt = install_dir.join(format!("ゆめ2っき{}", old_num));
        let new_dir_alt = install_dir.join(format!("ゆめ2っき{}", new_num));
        if old_dir_alt.exists() && old_dir_alt.is_dir() {
            eprintln!("  {} ディレクトリをリネーム中: {} -> {}", "→".cyan(), old_dir_alt.display(), new_dir_alt.display());
            fs::rename(&old_dir_alt, &new_dir_alt)?;
        }
    }

    if let Some(mut ver_state) = state.installed_versions.remove(old_ver) {
        ver_state.installed_patch = None;
        if !ver_state.install_history.contains(&new_ver) {
            ver_state.install_history.push(new_ver.clone());
        }
        state.installed_versions.insert(new_ver.clone(), ver_state);
    }

    if state.active_core.as_deref() == Some(old_ver) {
        state.active_core = Some(new_ver.clone());
    }

    state.installed_core = Some(new_ver.clone());
    state.installed_patch = None;
    if !state.install_history.contains(&new_ver) {
        state.install_history.push(new_ver.clone());
    }

    save_state(state)?;
    
    Ok(())
}

fn is_version_older(a: &str, b: &str) -> bool {
    let a_num = a.trim_start_matches("ver");
    let b_num = b.trim_start_matches("ver");

    let parse_ver = |s: &str| -> (u32, u32, char) {
        let re = regex::Regex::new(r"(\d+)\.(\d+)([a-z]?)").unwrap();
        if let Some(cap) = re.captures(s) {
            let major = cap[1].parse::<u32>().unwrap_or(0);
            let minor = cap[2].parse::<u32>().unwrap_or(0);
            let alpha = cap[3].chars().next().unwrap_or(' ');
            (major, minor, alpha)
        } else {
            (0, 0, ' ')
        }
    };

    let (a_maj, a_min, a_alp) = parse_ver(a_num);
    let (b_maj, b_min, b_alp) = parse_ver(b_num);

    if a_maj != b_maj {
        return a_maj < b_maj;
    }
    if a_min != b_min {
        return a_min < b_min;
    }
    a_alp < b_alp
}

fn extract_bundled_patch_number(entry: &UpdateEntry) -> Option<u32> {
    let mut text = entry.label.clone();
    if let Some(note) = &entry.note {
        text.push(' ');
        text.push_str(note);
    }

    let re_range = regex::Regex::new(r"パッチ\s*\d+\s*~\s*(\d+)\s*同梱").unwrap();
    if let Some(cap) = re_range.captures(&text) {
        if let Ok(num) = cap[1].parse::<u32>() {
            return Some(num);
        }
    }

    let re1 = regex::Regex::new(r"パッチ\s*(\d+)\s*(?:まで)?\s*(?:同梱|適用|導入|適用済み)").unwrap();
    if let Some(cap) = re1.captures(&text) {
        if let Ok(num) = cap[1].parse::<u32>() {
            return Some(num);
        }
    }

    let re_simple = regex::Regex::new(r"~\s*(\d+)\s*同梱").unwrap();
    if let Some(cap) = re_simple.captures(&text) {
        if let Ok(num) = cap[1].parse::<u32>() {
            return Some(num);
        }
    }

    let re2 = regex::Regex::new(r"(?i)patch\s*(\d+)").unwrap();
    if let Some(cap) = re2.captures(&text) {
        if let Ok(num) = cap[1].parse::<u32>() {
            return Some(num);
        }
    }

    None
}

fn normalize_patch_extracted_dir(install_dir: &PathBuf, core_dir: &PathBuf) -> Result<()> {
    let rd = fs::read_dir(install_dir)?;
    let mut subdirs = Vec::new();
    for entry in rd {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if name != "src" && name != "target" && name != "cfg" && name != ".git" && name != ".agents" 
                && !name.starts_with("ゆめ2っきver") && name != "ゆめ2っき" && name != "yume2kki" {
                subdirs.push(path);
            }
        }
    }

    for subdir in subdirs {
        let name = subdir.file_name().unwrap_or_default().to_string_lossy().to_string();
        let is_game_folder = name.contains("Yume2kki")
            || name.contains("ゆめ2っき")
            || name.contains("patch")
            || name.contains("パッチ")
            || name.contains("update");

        if is_game_folder {
            let real_root = find_patch_real_root(&subdir);
            eprintln!("  {} 検出された真のパッチルート: {}", "→".cyan(), real_root.display());
            eprintln!("  {} 展開されたパッチ階層を {} に適用中...", "→".cyan(), core_dir.display());
            
            let inner_rd = fs::read_dir(&real_root)?;
            let mut has_inner_yume2kki = false;
            let mut inner_yume2kki_path = PathBuf::new();
            
            let mut entries = Vec::new();
            for inner_entry in inner_rd {
                let inner_entry = inner_entry?;
                let inner_path = inner_entry.path();
                let inner_name = inner_path.file_name().unwrap_or_default().to_string_lossy().to_string();
                if inner_name == "ゆめ2っき" || inner_name == "yume2kki" {
                    has_inner_yume2kki = true;
                    inner_yume2kki_path = inner_path.clone();
                } else {
                    entries.push(inner_path);
                }
            }
            
            if has_inner_yume2kki {
                let target_yume2kki = core_dir.join("ゆめ2っき");
                if !target_yume2kki.exists() {
                    fs::create_dir_all(&target_yume2kki)?;
                }
                merge_directories(&inner_yume2kki_path, &target_yume2kki)?;
                
                for entry_path in entries {
                    let entry_name = entry_path.file_name().unwrap_or_default();
                    let dst_path = core_dir.join(entry_name);
                    if entry_path.is_dir() {
                        if !dst_path.exists() {
                            fs::create_dir_all(&dst_path)?;
                        }
                        merge_directories(&entry_path, &dst_path)?;
                    } else {
                        println!("  {} コピー中: {}", "→".cyan(), dst_path.display());
                        fs::copy(&entry_path, &dst_path)?;
                    }
                }
            } else {
                merge_directories(&real_root, core_dir)?;
            }
            
            let _ = fs::remove_dir_all(&subdir);
        }
    }

    Ok(())
}

fn merge_directories(src: &PathBuf, dst: &PathBuf) -> Result<()> {
    let rd = fs::read_dir(src)?;
    for entry in rd {
        let entry = entry?;
        let src_path = entry.path();
        let name = src_path.file_name().unwrap_or_default();
        let dst_path = dst.join(name);

        if src_path.is_dir() {
            if !dst_path.exists() {
                fs::create_dir_all(&dst_path)?;
            }
            merge_directories(&src_path, &dst_path)?;
        } else {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)?;
            }
            println!("  {} コピー中: {}", "→".cyan(), dst_path.display());
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn extract_archive(archive_path: &PathBuf, dest_dir: &PathBuf) -> Result<()> {
    let magic = {
        let mut f = fs::File::open(archive_path)?;
        let mut buf = [0u8; 8];
        let n = io::Read::read(&mut f, &mut buf).unwrap_or(0);
        buf[..n].to_vec()
    };

    let result = if magic.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        eprintln!("  {} 形式を検出: 7z", "→".cyan());
        extract_7z_preferring_external(archive_path, dest_dir)
    } else if magic.starts_with(&[0x50, 0x4B, 0x03, 0x04]) {
        eprintln!("  {} 形式を検出: zip", "→".cyan());
        extract_zip_preferring_external(archive_path, dest_dir)
    } else if magic.starts_with(&[0x1F, 0x8B]) {
        eprintln!("  {} 形式を検出: tar.gz", "→".cyan());
        extract_tar_gz_preferring_external(archive_path, dest_dir)
    } else if magic.starts_with(&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]) {
        eprintln!("  {} 形式を検出: tar.xz", "→".cyan());
        extract_tar_xz_preferring_external(archive_path, dest_dir)
    } else if magic.starts_with(&[0x42, 0x5A, 0x68]) {
        eprintln!("  {} 形式を検出: tar.bz2", "→".cyan());
        extract_tar_bz2_preferring_external(archive_path, dest_dir)
    } else {
        let name = archive_path.file_name().unwrap_or_default()
            .to_string_lossy().to_lowercase();
        if name.ends_with(".zip") {
            extract_zip_preferring_external(archive_path, dest_dir)
        } else if name.ends_with(".7z") {
            extract_7z_preferring_external(archive_path, dest_dir)
        } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            extract_tar_gz_preferring_external(archive_path, dest_dir)
        } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
            extract_tar_xz_preferring_external(archive_path, dest_dir)
        } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
            extract_tar_bz2_preferring_external(archive_path, dest_dir)
        } else {
            bail!(
                "未対応のアーカイブ形式です: {}\n対応形式: .zip / .7z / .tar.gz / .tar.xz / .tar.bz2",
                name
            )
        }
    };

    result?;
    Ok(())
}

// ============================================================
// 外部展開ツールの優先利用
// ============================================================

static CHILD_PID: OnceLock<Arc<Mutex<Option<u32>>>> = OnceLock::new();

fn child_pid_store() -> &'static Arc<Mutex<Option<u32>>> {
    CHILD_PID.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// 子プロセスを安全に終了する（クロスプラットフォーム対応）
fn terminate_child_process(pid: u32) {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        let mut killed = false;
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            unsafe {
                if libc::kill(pid as libc::pid_t, 0) != 0 {
                    killed = true;
                    break;
                }
            }
        }
        if !killed {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(&["/PID", &pid.to_string(), "/F", "/T"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

fn command_exists(cmd: &str, version_flag: &str) -> bool {
    Command::new(cmd)
        .arg(version_flag)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn run_external(cmd: &str, args: &[&str]) -> Result<()> {
    eprintln!(
        "  {} 外部コマンドで展開します: {} {}",
        "→".cyan(), cmd, args.join(" ")
    );

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("外部コマンドの起動に失敗しました: {}", cmd))?;

    if let Ok(mut guard) = child_pid_store().lock() {
        *guard = Some(child.id());
    }

    let status = child.wait()
        .with_context(|| format!("外部コマンドの待機に失敗しました: {}", cmd))?;

    if let Ok(mut guard) = child_pid_store().lock() {
        *guard = None;
    }

    if INTERRUPTED.load(Ordering::SeqCst) {
        bail!("__interrupted__");
    }

    if !status.success() {
        bail!(
            "外部コマンドが異常終了しました: {} (終了コード: {:?})",
            cmd, status.code()
        );
    }
    Ok(())
}

fn extract_with_external_then_fallback<F>(
    archive_path: &PathBuf,
    dest_dir: &PathBuf,
    cmd: &str,
    version_flag: &str,
    build_args: impl FnOnce(&PathBuf, &PathBuf) -> Vec<String>,
    rust_fallback: F,
) -> Result<()>
where
    F: FnOnce(&PathBuf, &PathBuf) -> Result<()>,
{
    if command_exists(cmd, version_flag) {
        let before = list_top_level_entries(dest_dir);
        let args_owned = build_args(archive_path, dest_dir);
        let args: Vec<&str> = args_owned.iter().map(|s| s.as_str()).collect();
        match run_external(cmd, &args) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if e.to_string().contains("__interrupted__") || INTERRUPTED.load(Ordering::SeqCst) {
                    return Err(e);
                }
                eprintln!(
                    "  {} 外部コマンドでの展開に失敗しました。内蔵の展開処理にフォールバックします。\n    {}",
                    "!".yellow().bold(), e
                );
                remove_new_top_level_entries(dest_dir, &before);
            }
        }
    }
    rust_fallback(archive_path, dest_dir)
}

fn extract_zip_preferring_external(archive_path: &PathBuf, dest_dir: &PathBuf) -> Result<()> {
    // macOS: unar がある場合はそちらを優先（Shift-JISファイル名を正しく処理できる）
    if cfg!(target_os = "macos") && command_exists("unar", "-v") {
        let before = list_top_level_entries(dest_dir);
        let args = vec![
            "-o".to_string(),
            dest_dir.to_string_lossy().to_string(),
            "-f".to_string(),
            "-e".to_string(), "shift_jis".to_string(),
            archive_path.to_string_lossy().to_string(),
        ];
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        match run_external("unar", &args_ref) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if e.to_string().contains("__interrupted__") || INTERRUPTED.load(Ordering::SeqCst) {
                    return Err(e);
                }
                eprintln!(
                    "  {} unar での展開に失敗しました。内蔵の展開処理にフォールバックします。\n    {}",
                    "!".yellow().bold(), e
                );
                remove_new_top_level_entries(dest_dir, &before);
            }
        }
    }

    // Linux: unzip -O SHIFT-JIS を使う
    // Windows / その他: -O フラグなしで unzip を試し、ダメなら内蔵フォールバック
    let build_args = if cfg!(target_os = "linux") {
        |archive_path: &PathBuf, dest_dir: &PathBuf| {
            vec![
                "-o".to_string(),
                "-O".to_string(), "SHIFT-JIS".to_string(),
                archive_path.to_string_lossy().to_string(),
                "-d".to_string(),
                dest_dir.to_string_lossy().to_string(),
            ]
        }
    } else {
        |archive_path: &PathBuf, dest_dir: &PathBuf| {
            vec![
                "-o".to_string(),
                archive_path.to_string_lossy().to_string(),
                "-d".to_string(),
                dest_dir.to_string_lossy().to_string(),
            ]
        }
    };

    extract_with_external_then_fallback(
        archive_path,
        dest_dir,
        "unzip",
        "-v",
        build_args,
        extract_zip,
    )
}

fn extract_7z_preferring_external(archive_path: &PathBuf, dest_dir: &PathBuf) -> Result<()> {
    for cmd in ["7z", "7zr", "7za"] {
        if command_exists(cmd, "--help") {
            let before = list_top_level_entries(dest_dir);
            let args = vec![
                "x".to_string(),
                "-y".to_string(),
                format!("-o{}", dest_dir.to_string_lossy()),
                archive_path.to_string_lossy().to_string(),
            ];
            let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            match run_external(cmd, &args_ref) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if e.to_string().contains("__interrupted__") || INTERRUPTED.load(Ordering::SeqCst) {
                        return Err(e);
                    }
                    eprintln!(
                        "  {} {} での展開に失敗しました。内蔵の展開処理にフォールバックします。\n    {}",
                        "!".yellow().bold(), cmd, e
                    );
                    remove_new_top_level_entries(dest_dir, &before);
                }
            }
            return Ok(());
        }
    }
    extract_7z(archive_path, dest_dir)
}

fn extract_tar_gz_preferring_external(archive_path: &PathBuf, dest_dir: &PathBuf) -> Result<()> {
    extract_with_external_then_fallback(
        archive_path,
        dest_dir,
        "tar",
        "--version",
        |archive_path, dest_dir| {
            vec![
                "-xzf".to_string(),
                archive_path.to_string_lossy().to_string(),
                "-C".to_string(),
                dest_dir.to_string_lossy().to_string(),
            ]
        },
        |p, d| extract_tar(p, d, flate2::read::GzDecoder::new),
    )
}

fn extract_tar_xz_preferring_external(archive_path: &PathBuf, dest_dir: &PathBuf) -> Result<()> {
    extract_with_external_then_fallback(
        archive_path,
        dest_dir,
        "tar",
        "--version",
        |archive_path, dest_dir| {
            vec![
                "-xJf".to_string(),
                archive_path.to_string_lossy().to_string(),
                "-C".to_string(),
                dest_dir.to_string_lossy().to_string(),
            ]
        },
        extract_tar_xz,
    )
}

fn extract_tar_bz2_preferring_external(archive_path: &PathBuf, dest_dir: &PathBuf) -> Result<()> {
    extract_with_external_then_fallback(
        archive_path,
        dest_dir,
        "tar",
        "--version",
        |archive_path, dest_dir| {
            vec![
                "-xjf".to_string(),
                archive_path.to_string_lossy().to_string(),
                "-C".to_string(),
                dest_dir.to_string_lossy().to_string(),
            ]
        },
        |p, d| extract_tar(p, d, bzip2::read::BzDecoder::new),
    )
}

fn extract_zip(zip_path: &PathBuf, dest_dir: &PathBuf) -> Result<()> {
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let total = archive.len() as u64;
    let pb = make_extract_pb(total, "files");

    for i in 0..archive.len() {
        if INTERRUPTED.load(Ordering::SeqCst) { bail!("__interrupted__"); }
        let mut entry = archive.by_index(i)?;
        let raw_bytes = entry.name_raw();
        let (decoded_str, _, has_error) = SHIFT_JIS.decode(raw_bytes);
        let safe_name = if has_error {
            entry.mangled_name()
        } else {
            PathBuf::from(decoded_str.into_owned())
        };
        let out_path = dest_dir.join(safe_name);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out_file = fs::File::create(&out_path)?;
            io::copy(&mut entry, &mut out_file)?;
        }
        pb.inc(1);
    }
    pb.finish_and_clear();
    Ok(())
}

fn extract_7z(archive_path: &PathBuf, dest_dir: &PathBuf) -> Result<()> {
    let archive_size = fs::metadata(archive_path).map(|m| m.len()).unwrap_or(0);
    let pb = make_extract_pb(archive_size, "bytes");
    sevenz_rust::decompress_file_with_extract_fn(archive_path, dest_dir, |entry, reader, dest| {
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Err(sevenz_rust::Error::other("__interrupted__"));
        }
        let result = sevenz_rust::default_entry_extract_fn(entry, reader, dest);
        pb.inc(entry.compressed_size);
        result
    })
    .map_err(|e| anyhow::anyhow!(
        "7z 展開に失敗しました: {} ({})", archive_path.display(), e
    ))?;
    pb.finish_and_clear();
    Ok(())
}

fn extract_tar<D, F>(archive_path: &PathBuf, dest_dir: &PathBuf, make_decoder: F) -> Result<()>
where
    D: io::Read,
    F: FnOnce(fs::File) -> D,
{
    let archive_size = fs::metadata(archive_path).map(|m| m.len()).unwrap_or(0);
    let pb = make_extract_pb(archive_size, "bytes");

    struct ProgressReader<R: io::Read> { inner: R, pb: ProgressBar }
    impl<R: io::Read> io::Read for ProgressReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if INTERRUPTED.load(Ordering::SeqCst) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "__interrupted__"));
            }
            let n = self.inner.read(buf)?;
            self.pb.inc(n as u64);
            Ok(n)
        }
    }

    let file    = fs::File::open(archive_path)?;
    let decoder = make_decoder(file);
    let tracked = ProgressReader { inner: decoder, pb: pb.clone() };
    let mut archive = tar::Archive::new(tracked);
    archive.unpack(dest_dir)
        .with_context(|| format!("tar 展開に失敗しました: {}", archive_path.display()))?;
    pb.finish_and_clear();
    Ok(())
}

fn extract_tar_xz(archive_path: &PathBuf, dest_dir: &PathBuf) -> Result<()> {
    extract_tar(archive_path, dest_dir, |f| xz2::read::XzDecoder::new(f))
}

// ============================================================
// 表示ヘルパー
// ============================================================

fn print_entries(entries: &[UpdateEntry], count: usize) {
    for entry in entries.iter().take(count) {
        let (icon, kind_label) = match entry.kind {
            EntryKind::Core   => ("●".green().bold(),   "core  ".green().bold()),
            EntryKind::Patch  => ("○".blue().bold(),    "patch ".blue().bold()),
            EntryKind::Runner => ("→".magenta().bold(), "走者  ".magenta().bold()),
            EntryKind::Other  => ("·".dimmed().bold(),  "その他".dimmed().bold()),
        };
        let auth = if entry.authors.is_empty() {
            String::new()
        } else {
            format!("  （{}）", entry.authors.join("/"))
        };
        println!(
            "  {} {} [{}] {}{}",
            entry.date.dimmed(), icon, kind_label, entry.label.yellow(), auth.dimmed()
        );
        if let Some(note) = &entry.note {
            println!("             {}", note.red().dimmed());
        }
    }
    if entries.len() > count {
        println!(
            "\n  {}",
            format!("… 他{}件  (--count N で表示数変更)", entries.len() - count).dimmed()
        );
    }
}

// ============================================================
// コマンド実装
// ============================================================

fn cmd_update(count: Option<usize>) -> Result<()> {
    println!("{}", "ゆめ2っき パッケージマネージャー".cyan().bold());
    println!("{}", "パッケージリストを更新しています...".dimmed());

    let entries = fetch_updates()?;
    let total   = entries.len();
    let mut state = load_state();

    let new_count_msg = if let Some(prev) = &state.last_fetched {
        let cnt = entries
            .iter()
            .filter(|e| !state.entries.iter().any(|o| o.date == e.date && o.label == e.label))
            .count();
        format!("{} 件新着 / 前回同期: {}", cnt, prev)
    } else {
        format!("{} 件取得", total)
    };

    state.entries = entries;
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    state.last_fetched = Some(now.clone());
    save_state(&state)?;

    println!(
        "{} パッケージリストを更新しました  {} ({})",
        "✓".green().bold(), new_count_msg.yellow(), now.dimmed()
    );
    println!();
    println!("{}", "最近の更新一覧".cyan().bold());
    println!("{}", "─".repeat(60).dimmed());
    let display_count = count.unwrap_or_else(|| default_display_count(&state.entries));
    print_entries(&state.entries, display_count);
    println!();
    println!("  {}", "y2k upgrade            最新バージョンを確認".dimmed());
    println!("  {}", "y2k install core       本体をインストール".dimmed());
    Ok(())
}

fn cmd_upgrade(download: bool) -> Result<()> {
    let mut state = load_state();
    let mut config = load_config();
    if state.entries.is_empty() {
        println!("{}", "更新情報がありません。先に `y2k update` を実行してください。".yellow());
        return Ok(());
    }

    let latest_installed_ver = if state.installed_versions.is_empty() {
        None
    } else {
        let mut keys: Vec<&String> = state.installed_versions.keys().collect();
        keys.sort_by(|a, b| b.cmp(a));
        Some(keys[0].clone())
    };

    let latest_core  = state.entries.iter().find(|e| e.kind == EntryKind::Core);
    let latest_patch = state.entries.iter().find(|e| e.kind == EntryKind::Patch);

    let mut display_core_ver = latest_core.map(|e| e.label.clone());
    if let Some(core_entry) = latest_core {
        let core_idx = state.entries.iter().position(|e| e.label == core_entry.label && e.kind == EntryKind::Core);
        if let Some(c_idx) = core_idx {
            let next_core_idx = state.entries[..c_idx].iter()
                .rposition(|e| e.kind == EntryKind::Core);

            let start_range = match next_core_idx {
                Some(idx) => idx + 1,
                None => 0,
            };

            let range_entries = &state.entries[start_range..c_idx];
            let up_patch = range_entries.iter()
                .find(|e| e.kind == EntryKind::Patch && e.label.contains("アップデートパッチ"));

            if let Some(patch) = up_patch {
                let re = regex::Regex::new(r"ver\d+\.\d+[a-z]?").unwrap();
                if let Some(cap) = re.captures(&patch.label) {
                    display_core_ver = Some(cap[0].to_string());
                }
            }
        }
    }

    println!("{}", "最新バージョン情報".cyan().bold());
    println!("{}", "─".repeat(50).dimmed());

    if let Some(core_ver) = &display_core_ver {
        if let Some(entry) = latest_core {
            println!("{} {} {}  （{}）",
                "●".green(), "core  ".bold(), core_ver.yellow().bold(), entry.date.dimmed());
            if !entry.authors.is_empty() {
                println!("       担当: {}", entry.authors.join(", ").dimmed());
            }
            if download {
                println!("       DL:   {}", entry.dl_url.as_deref().unwrap_or(WIKI_URL).cyan());
            }
        }
    }

    if let Some(entry) = latest_patch {
        println!("{} {} {}  （{}）",
            "○".blue(), "patch ".bold(), entry.label.yellow(), entry.date.dimmed());
        if !entry.authors.is_empty() {
            println!("       担当: {}", entry.authors.join(", ").dimmed());
        }
        if let Some(note) = &entry.note {
            println!("       注記: {}", note.red().dimmed());
        }
        if download {
            println!("       DL:   {}", entry.dl_url.as_deref().unwrap_or(WIKI_URL).cyan());
        }
    }

    println!("{}", "─".repeat(50).dimmed());

    if let Some(installed) = &latest_installed_ver {
        if let Some(core_entry) = latest_core {
            if is_version_older(installed, &core_entry.label) {
                println!("{} core: {} → {} にアップデート可能です",
                    "!".yellow().bold(), installed.dimmed(), core_entry.label.green().bold());
                println!("アップデートを実行します...\n");
                return cmd_install_core_version(&mut state, &mut config, None);
            }
        }

        if let Some(patch) = latest_patch {
            let ver_state = state.installed_versions.get(installed).unwrap();
            let has_new_patch = ver_state.installed_patch.as_deref() != Some(patch.label.as_str());
            
            if has_new_patch {
                println!("{} patch: {} にアップデート可能です (適用対象: {})",
                    "!".yellow().bold(), patch.label.green().bold(), installed.yellow());
                println!("アップデートを実行します...\n");
                
                state.active_core = Some(installed.clone());
                save_state(&state)?;
                return cmd_install_patch(&mut state, &mut config);
            }
        }

        println!("{} すべて最新バージョンです (core: {}, patch: {})",
            "✓".green().bold(),
            installed.yellow(),
            state.installed_versions.get(installed)
                .and_then(|s| s.installed_patch.as_deref())
                .unwrap_or("なし").yellow()
        );
    } else {
        println!("{}", "coreがインストールされていません。".dimmed());
        println!("  インストール: {}", "y2k install core".cyan());
    }

    if !download && latest_installed_ver.is_none() {
        println!("\n  {}", "DLリンク表示: y2k upgrade --download".dimmed());
    }
    Ok(())
}

fn download_patch_only(
    entry: &UpdateEntry,
    cleanup_target: &Arc<Mutex<CleanupTarget>>,
) -> Result<PathBuf> {
    let raw_url = entry.dl_url.as_deref()
        .context("パッチのDLリンクが見つかりません。Wikiを直接確認してください。")?;

    let tmp_path = std::env::temp_dir()
        .join(format!("y2k-patch-{}.bin", entry.label));

    if let Ok(mut target) = cleanup_target.lock() {
        *target = CleanupTarget::DownloadTmpFile(tmp_path.clone());
    }

    let hint_dir = std::env::temp_dir();
    let hint_filename = format!("y2k-patch-{}", entry.label);
    let hint_path = hint_dir.join(&hint_filename);

    let exts = [".bin", ".zip", ".7z", ".tar.gz", ".tar.xz", ".tar.bz2"];
    let cached = exts.iter().find_map(|ext| {
        let p = std::env::temp_dir()
            .join(format!("y2k-patch-{}{}", entry.label, ext));
        if p.exists() && fs::metadata(&p).map(|m| m.len()).unwrap_or(0) > 10 * 1024 {
            Some(p)
        } else {
            None
        }
    }).or_else(|| {
        let ver     = &entry.label;
        let ver_num = ver.trim_start_matches("パッチ");
        let tmp_dir = std::env::temp_dir();
        fs::read_dir(&tmp_dir).ok()?.find_map(|e| {
            let e       = e.ok()?;
            let fname = e.file_name().to_string_lossy().to_string();
            let fname_lower = fname.to_lowercase();
            let is_archive = exts.iter().any(|ext| fname_lower.ends_with(ext));
            let is_y2k_related = fname_lower.contains("yume")
                || fname_lower.contains("y2k")
                || fname_lower.contains("patch")
                || fname_lower.contains("パッチ")
                || fname_lower.contains("update");
            let is_core = fname_lower.contains("core");
            
            let has_ver = fname.contains(ver)
                || fname_lower.contains(&format!("patch{}", ver_num))
                || fname_lower.contains(&format!("patch_{}", ver_num))
                || fname_lower.contains(&format!("p{}", ver_num));

            if is_archive && has_ver && is_y2k_related && !is_core
                && fs::metadata(e.path()).map(|m| m.len()).unwrap_or(0) > 10 * 1024
            {
                Some(e.path())
            } else {
                None
            }
        })
    });

    let zip_path = if let Some(cached_path) = cached {
        println!("  {} キャッシュされたパッチファイルを使用します: {}",
            "✓".green().bold(), cached_path.display().to_string().dimmed());
        cached_path
    } else {
        println!("  {} ダウンロード中: {}", "→".cyan(), raw_url.dimmed());
        match download_file(raw_url, Some(&hint_path)) {
            Err(e) => {
                if INTERRUPTED.load(Ordering::SeqCst) {
                    return Err(anyhow::anyhow!("__interrupted__"));
                }
                if tmp_path.exists() { let _ = fs::remove_file(&tmp_path); }
                eprintln!("\n{} パッチの自動ダウンロードに失敗しました:", "✗".red().bold());
                eprintln!("  {}", e.to_string().dimmed());
                println!("\n{}", "手動ダウンロード手順:".yellow().bold());
                println!("  1. ブラウザで以下のURLを開く:");
                println!("       {}", raw_url.cyan().bold());
                let tmp_dir = std::env::temp_dir();
                println!("  2. ダウンロードしたファイルを {} 以下に置く", tmp_dir.display());
                println!("  3. 再度インストーラーを実行する");
                bail!("パッチの自動ダウンロード失敗: {}", e);
            }
            Ok(actual_path) => {
                println!("  {} ダウンロード完了: {}",
                    "✓".green().bold(),
                    actual_path.file_name().unwrap_or_default()
                        .to_string_lossy().yellow());
                actual_path
            }
        }
    };

    let fsize = fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
    if fsize < 10 * 1024 {
        let _ = fs::remove_file(&zip_path);
        bail!("ダウンロードされたパッチファイルが小さすぎます ({} bytes)。", fsize);
    }

    Ok(zip_path)
}

fn extract_patch_only(
    zip_path: &PathBuf,
    install_dir: &PathBuf,
    cleanup_target: &Arc<Mutex<CleanupTarget>>,
    active_core: Option<&str>,
) -> Result<()> {
    fs::create_dir_all(install_dir)?;

    let before_extract = list_top_level_entries(install_dir);
    if let Ok(mut target) = cleanup_target.lock() {
        *target = CleanupTarget::ExtractDiff {
            dir: install_dir.clone(),
            before: before_extract,
        };
    }

    if let Err(e) = extract_archive(zip_path, install_dir) {
        if e.to_string().contains("__interrupted__") || INTERRUPTED.load(Ordering::SeqCst) {
            if let Ok(target) = cleanup_target.lock() {
                target.cleanup();
            }
            return Ok(());
        }
        return Err(e).context("パッチアーカイブの展開に失敗しました");
    }

    let core_dir = find_active_core_dir(install_dir, active_core);
    if core_dir != *install_dir {
        normalize_patch_extracted_dir(install_dir, &core_dir)?;
    }

    let _ = fs::remove_file(zip_path);
    println!("  {} 展開と上書きが完了しました", "✓".green().bold());
    println!();
    Ok(())
}

fn apply_patch_internal(
    entry: &UpdateEntry,
    install_dir: &PathBuf,
    cleanup_target: &Arc<Mutex<CleanupTarget>>,
    active_core: Option<&str>,
) -> Result<()> {
    let zip_path = download_patch_only(entry, cleanup_target)?;
    extract_patch_only(&zip_path, install_dir, cleanup_target, active_core)
}

fn sync_package_list_if_empty(state: &mut State) -> Result<()> {
    if state.entries.is_empty() {
        println!("{}", "パッケージリストが空です。自動で update を実行します...".yellow());
        let entries = fetch_updates()?;
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        state.entries = entries;
        state.last_fetched = Some(now.clone());
        save_state(state)?;
        println!("{} パッケージリストを更新しました ({})", "✓".green().bold(), now.dimmed());
    }
    Ok(())
}

enum CleanupTarget {
    DownloadTmpFile(PathBuf),
    ExtractDiff {
        dir: PathBuf,
        before: std::collections::HashSet<PathBuf>,
    },
}

impl CleanupTarget {
    fn cleanup(&self) {
        match self {
            CleanupTarget::DownloadTmpFile(p) => {
                if p.exists() {
                    let _ = fs::remove_file(p);
                    eprintln!(
                        "\n{} 中断されました。一時ファイルを削除しました: {}",
                        "!".yellow().bold(), p.display()
                    );
                    return;
                }
                eprintln!("\n{} 中断されました。", "!".yellow().bold());
            }
            CleanupTarget::ExtractDiff { dir, before } => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                
                let removed = diff_new_top_level_entries(dir, before);
                if removed.is_empty() {
                    eprintln!("\n{} 中断されました。", "!".yellow().bold());
                } else {
                    let mut success_all = true;
                    for p in &removed {
                        if p.is_dir() {
                            if let Err(e) = fs::remove_dir_all(p) {
                                std::thread::sleep(std::time::Duration::from_millis(250));
                                if fs::remove_dir_all(p).is_err() {
                                    eprintln!("  {} ディレクトリの自動削除に失敗: {} (原因: {})", "!".red(), p.display(), e);
                                    success_all = false;
                                }
                            }
                        } else if let Err(e) = fs::remove_file(p) {
                            std::thread::sleep(std::time::Duration::from_millis(250));
                            if fs::remove_file(p).is_err() {
                                eprintln!("  {} ファイルの自動削除に失敗: {} (原因: {})", "!".red(), p.display(), e);
                                success_all = false;
                            }
                        }
                    }
                    if success_all {
                        eprintln!(
                            "\n{} 中断されました。展開中に作成された以下の不完全な項目を削除しました:",
                            "!".yellow().bold()
                        );
                        for p in &removed {
                            eprintln!("    {}", p.display().to_string().dimmed());
                        }
                    } else {
                        eprintln!("\n{} 中断されましたが、一部の差分エントリの自動削除に失敗しました。手動で確認してください。", "!".yellow().bold());
                    }
                }
            }
        }
    }
}

fn cmd_install_core_version(
    state: &mut State,
    config: &mut Config,
    target_version: Option<&str>,
) -> Result<()> {
    let install_dir = match &config.install_dir {
        Some(d) => {
            let p = PathBuf::from(d);
            p.canonicalize().unwrap_or(p)
        }
        None => {
            println!("{}", "インストール先ディレクトリが設定されていません。".yellow());
            prompt_install_dir(config)?
        }
    };

    sync_package_list_if_empty(state)?;

    let entry = if let Some(target) = target_version {
        let normalized_target = target.trim_start_matches("ver");
        state.entries.iter()
            .find(|e| e.kind == EntryKind::Core && (e.label == normalized_target || e.label == format!("ver{}", normalized_target) || e.label.contains(normalized_target)))
            .context(format!("指定されたバージョン {} の core エントリが見つかりません。", target))?
            .clone()
    } else {
        state.entries.iter()
            .find(|e| e.kind == EntryKind::Core)
            .context("coreのエントリが見つかりません。Wikiの形式が変わった可能性があります。")?
            .clone()
    };

    let raw_url = entry.dl_url.as_deref()
        .context("DLリンクが見つかりません。Wikiを直接確認してください。")?;

    println!("{}", "core をインストールします".cyan().bold());
    println!("  バージョン : {}", entry.label.yellow().bold());
    println!("  展開先     : {}", install_dir.display().to_string().cyan());
    println!();

    let tmp_path = std::env::temp_dir()
        .join(format!("y2k-core-{}.bin", entry.label));

    let cleanup_target: Arc<Mutex<CleanupTarget>> =
        Arc::new(Mutex::new(CleanupTarget::DownloadTmpFile(tmp_path.clone())));

    let cleanup_target_for_handler = cleanup_target.clone();

    let _ = ctrlc::set_handler(move || {
        INTERRUPTED.store(true, Ordering::SeqCst);

        if let Ok(pid_guard) = child_pid_store().lock() {
            if let Some(pid) = *pid_guard {
                terminate_child_process(pid);
            }
        }

        if let Ok(target) = cleanup_target_for_handler.lock() {
            target.cleanup();
        } else {
            eprintln!("\n{} 中断されました。", "!".yellow().bold());
        }
        std::process::exit(130);
    });

    let exts = [".bin", ".zip", ".7z", ".tar.gz", ".tar.xz", ".tar.bz2"];
    let cached = exts.iter().find_map(|ext| {
        let p = std::env::temp_dir()
            .join(format!("y2k-core-{}{}", entry.label, ext));
        if p.exists() && fs::metadata(&p).map(|m| m.len()).unwrap_or(0) > 1024 * 1024 {
            Some(p)
        } else {
            None
        }
    }).or_else(|| {
        let ver     = &entry.label;
        let tmp_dir = std::env::temp_dir();
        fs::read_dir(&tmp_dir).ok()?.find_map(|e| {
            let e       = e.ok()?;
            let fname = e.file_name().to_string_lossy().to_string();
            let is_archive = exts.iter().any(|ext| fname.to_lowercase().ends_with(ext));
            let has_ver    = fname.contains(ver.trim_start_matches("ver"));
            if is_archive && has_ver
                && fs::metadata(e.path()).map(|m| m.len()).unwrap_or(0) > 1024 * 1024
            {
                Some(e.path())
            } else {
                None
            }
        })
    });

    let zip_path = if let Some(cached_path) = cached {
        println!("  {} キャッシュされたファイルを使用します: {}",
            "✓".green().bold(), cached_path.display().to_string().dimmed());
        cached_path
    } else {
        println!("  {} ダウンロード中: {}", "→".cyan(), raw_url.dimmed());
        match download_file(raw_url, None) {
            Err(e) => {
                if INTERRUPTED.load(Ordering::SeqCst) { return Ok(()); }
                if tmp_path.exists() { let _ = fs::remove_file(&tmp_path); }
                eprintln!("\n{} 自動ダウンロードに失敗しました:", "✗".red().bold());
                eprintln!("  {}", e.to_string().dimmed());
                println!("\n{}", "手動ダウンロード手順:".yellow().bold());
                println!("  1. ブラウザで以下のURLを開く:");
                println!("       {}", raw_url.cyan().bold());
                let tmp_dir = std::env::temp_dir();
                println!("  2. ダウンロードしたファイルを {} 以下に置く", tmp_dir.display());
                println!("     (ファイル名はそのまま、拡張子は .zip .7z なども可)");
                println!("  3. 再度 `y2k install core` を実行する");
                return Ok(());
            }
            Ok(actual_path) => {
                println!("  {} ダウンロード完了: {}",
                    "✓".green().bold(),
                    actual_path.file_name().unwrap_or_default()
                        .to_string_lossy().yellow());
                actual_path
            }
        }
    };

    let fsize = fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
    if fsize < 1024 * 1024 {
        let _ = fs::remove_file(&zip_path);
        bail!("ダウンロードされたファイルが小さすぎます ({} bytes)。", fsize);
    }

    // 関連するパッチの自動ダウンロードを先に行う
    let mut downloaded_patches = Vec::new();
    if target_version.is_none() {
        let core_idx = state.entries.iter().position(|e| e.label == entry.label && e.kind == EntryKind::Core);
        if let Some(c_idx) = core_idx {
            // c_idx より新しい（インデックスが小さい）エントリのうち、最も近い「次の Core」の位置を探す
            let next_core_idx = state.entries[..c_idx].iter()
                .rposition(|e| e.kind == EntryKind::Core);

            // 対象とする範囲は、next_core_idx（あればその直後、なければ 0）から c_idx までの間
            let start_range = match next_core_idx {
                Some(idx) => idx + 1,
                None => 0,
            };

            // この範囲内からパッチを抽出する
            let range_entries = &state.entries[start_range..c_idx];
            
            // この範囲の中の、最も新しい「アップデートパッチ」の位置を探す
            let up_idx = range_entries.iter()
                .position(|e| e.kind == EntryKind::Patch && e.label.contains("アップデートパッチ"));

            let mut patches_to_apply: Vec<UpdateEntry> = if let Some(u_idx) = up_idx {
                range_entries[u_idx..].iter()
                    .filter(|e| e.kind == EntryKind::Patch)
                    .cloned()
                    .collect()
            } else {
                range_entries.iter()
                    .filter(|e| e.kind == EntryKind::Patch)
                    .cloned()
                    .collect()
            };

            let mut max_bundled_patch: Option<u32> = None;
            for p in &patches_to_apply {
                if p.label.contains("アップデートパッチ") || p.label.contains("core") {
                    if let Some(num) = extract_bundled_patch_number(p) {
                        if max_bundled_patch.map_or(true, |m| num > m) {
                            max_bundled_patch = Some(num);
                            println!("  {} {} に同梱のパッチ上限を検出: パッチ {}", "→".cyan(), p.label.yellow(), num);
                        }
                    }
                }
            }

            if let Some(limit) = max_bundled_patch {
                let re_patch_num = regex::Regex::new(r"パッチ\s*(\d+)").unwrap();
                patches_to_apply.retain(|p| {
                    if p.label.contains("アップデートパッチ") {
                        return true;
                    }
                    if let Some(cap) = re_patch_num.captures(&p.label) {
                        if let Ok(num) = cap[1].parse::<u32>() {
                            if num <= limit {
                                println!("  {} {} は同梱されているためインストールをスキップします", "→".dimmed(), p.label.dimmed());
                                return false;
                            }
                        }
                    }
                    true
                });
            }

            if !patches_to_apply.is_empty() {
                println!("\n{}", "coreに関連するパッチを先にすべてダウンロードします...".cyan().bold());
                for patch in patches_to_apply.into_iter().rev() {
                    let patch_zip_path = download_patch_only(&patch, &cleanup_target)?;
                    downloaded_patches.push((patch, patch_zip_path));
                }
            }
    }
}

    // coreの展開を開始
    fs::create_dir_all(&install_dir)?;

    let before_extract = list_top_level_entries(&install_dir);
    if let Ok(mut target) = cleanup_target.lock() {
        *target = CleanupTarget::ExtractDiff {
            dir: install_dir.clone(),
            before: before_extract,
        };
    }

    println!("\n{} core の展開を開始します...", "→".cyan());
    if let Err(e) = extract_archive(&zip_path, &install_dir) {
        if e.to_string().contains("__interrupted__") || INTERRUPTED.load(Ordering::SeqCst) {
            if let Ok(target) = cleanup_target.lock() {
                target.cleanup();
            }
            return Ok(());
        }
        return Err(e).context("アーカイブ展開に失敗しました");
    }

    let _ = fs::remove_file(&zip_path);
    println!("  {} 展開完了", "✓".green().bold());
    println!();

    state.active_core = Some(entry.label.clone());
    let ver_state = state.installed_versions
        .entry(entry.label.clone())
        .or_insert_with(InstalledVersionState::default);

    if !ver_state.install_history.contains(&entry.label) {
        ver_state.install_history.push(entry.label.clone());
    }

    state.installed_core = Some(entry.label.clone());
    if !state.install_history.contains(&entry.label) {
        state.install_history.push(entry.label.clone());
    }
    save_state(state)?;

    println!("{} core {} をインストールしました",
        "✓".green().bold(), entry.label.yellow().bold());

    if !downloaded_patches.is_empty() {
        println!("\n{}", "coreに関連するアップデートパッチおよびそれ以前のパッチを順次適用します...".cyan().bold());
        let mut current_core_ver = entry.label.clone();
        for (patch, patch_zip_path) in downloaded_patches {
            println!("{} patch {} を適用中...", "→".cyan(), patch.label.yellow().bold());
            extract_patch_only(&patch_zip_path, &install_dir, &cleanup_target, Some(&current_core_ver))?;

            let ver_state = state.installed_versions
                .get_mut(&current_core_ver)
                .context("バージョン状態が見つかりません")?;
            ver_state.installed_patch = Some(patch.label.clone());
            if !ver_state.install_history.contains(&patch.label) {
                ver_state.install_history.push(patch.label.clone());
            }

            state.installed_patch = Some(patch.label.clone());
            if !state.install_history.contains(&patch.label) {
                state.install_history.push(patch.label.clone());
            }
            save_state(state)?;

            if patch.label.contains("アップデートパッチ") || patch.label.to_lowercase().contains("update") {
                let re = regex::Regex::new(r"ver\d+\.\d+[a-z]?").unwrap();
                if let Some(cap) = re.captures(&patch.label) {
                    let new_ver = cap[0].to_string();
                    if new_ver != current_core_ver {
                        if let Err(e) = try_promote_version(state, &install_dir, &current_core_ver, &patch.label) {
                            eprintln!("警告: バージョン昇格処理に失敗しました: {}", e);
                        } else {
                            current_core_ver = new_ver;
                        }
                    }
                }
            }
            println!("{} patch {} を適用しました", "✓".green().bold(), patch.label.yellow().bold());
        }
    }

    Ok(())
}

fn cmd_install_patch(state: &mut State, config: &mut Config) -> Result<()> {
    let install_dir = match &config.install_dir {
        Some(d) => {
            let p = PathBuf::from(d);
            p.canonicalize().unwrap_or(p)
        }
        None => {
            println!("{}", "インストール先ディレクトリが設定されていません。".yellow());
            prompt_install_dir(config)?
        }
    };

    let active_core = state.active_core.clone()
        .context("アクティブな core がありません。先に `y2k install core` で本体をインストールしてください。")?;

    sync_package_list_if_empty(state)?;

    let entry = state.entries.iter()
        .find(|e| e.kind == EntryKind::Patch)
        .context("最新パッチのエントリが見つかりません。")?
        .clone();

    println!("{}", "patch をインストールします".cyan().bold());
    println!("  バージョン : {}", entry.label.yellow().bold());
    println!("  適用対象本体: {}", active_core.yellow().bold());
    println!("  展開先     : {}", install_dir.display().to_string().cyan());
    println!();

    let tmp_path = std::env::temp_dir()
        .join(format!("y2k-patch-{}.bin", entry.label));

    let cleanup_target: Arc<Mutex<CleanupTarget>> =
        Arc::new(Mutex::new(CleanupTarget::DownloadTmpFile(tmp_path.clone())));

    let cleanup_target_for_handler = cleanup_target.clone();

    let _ = ctrlc::set_handler(move || {
        INTERRUPTED.store(true, Ordering::SeqCst);
        if let Ok(pid_guard) = child_pid_store().lock() {
            if let Some(pid) = *pid_guard {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
                let mut killed = false;
                for _ in 0..10 {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    unsafe {
                        if libc::kill(pid as libc::pid_t, 0) != 0 {
                            killed = true;
                            break;
                        }
                    }
                }
                if !killed {
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL); }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
        if let Ok(target) = cleanup_target_for_handler.lock() {
            target.cleanup();
        }
        std::process::exit(130);
    });

    apply_patch_internal(&entry, &install_dir, &cleanup_target, Some(&active_core))?;

    let ver_state = state.installed_versions
        .entry(active_core.clone())
        .or_insert_with(InstalledVersionState::default);
    ver_state.installed_patch = Some(entry.label.clone());
    if !ver_state.install_history.contains(&entry.label) {
        ver_state.install_history.push(entry.label.clone());
    }

    state.installed_patch = Some(entry.label.clone());
    if !state.install_history.contains(&entry.label) {
        state.install_history.push(entry.label.clone());
    }
    save_state(state)?;

    if entry.label.contains("アップデートパッチ") || entry.label.to_lowercase().contains("update") {
        if let Err(e) = try_promote_version(state, &install_dir, &active_core, &entry.label) {
            eprintln!("警告: バージョン昇格処理に失敗しました: {}", e);
        }
    }

    println!("{} patch {} を適用しました", "✓".green().bold(), entry.label.yellow().bold());
    Ok(())
}

fn cmd_install(kind: &str) -> Result<()> {
    let mut state   = load_state();
    let mut config = load_config();

    if kind.contains('@') {
        let parts: Vec<&str> = kind.split('@').collect();
        if parts.len() == 2 {
            let k = parts[0];
            let v = parts[1];
            if k == "core" {
                return cmd_install_core_version(&mut state, &mut config, Some(v));
            } else {
                bail!("バージョン指定ができるのは core ののみです (例: core@0.129b)");
            }
        }
    }

    match kind {
        "core"  => cmd_install_core_version(&mut state, &mut config, None),
        "patch" => cmd_install_patch(&mut state, &mut config),
        other   => bail!("不明なkind: {}  (core / patch または core@バージョン を指定してください)", other),
    }
}

fn cmd_list() -> Result<()> {
    let state = load_state();
    println!("{}", "インストール済みのバージョン一覧".cyan().bold());
    println!("{}", "─".repeat(50).dimmed());

    if state.installed_versions.is_empty() {
        println!("{}", "  インストール済みのバージョンはありません。".dimmed());
    } else {
        let mut versions: Vec<(&String, &InstalledVersionState)> = state.installed_versions.iter().collect();
        versions.sort_by(|a, b| b.0.cmp(a.0)); // バージョン降順

        for (ver, ver_state) in versions {
            let is_active = state.active_core.as_deref() == Some(ver.as_str());
            let active_mark = if is_active {
                "  * ".green().bold().to_string()
            } else {
                "    ".to_string()
            };

            let patch_info = match &ver_state.installed_patch {
                Some(p) => format!(" (パッチ: {})", p.yellow()),
                None => " (パッチなし)".dimmed().to_string(),
            };

            let active_label = if is_active {
                format!(" {}", "[アクティブ]".green().bold())
            } else {
                "".to_string()
            };

            println!("{}{}{}{}", active_mark, ver.yellow().bold(), patch_info, active_label);
        }
    }
    Ok(())
}

fn cmd_show(count: Option<usize>) -> Result<()> {
    let state = load_state();
    if state.entries.is_empty() {
        println!("{}", "更新情報がありません。先に `y2k update` を実行してください。".yellow());
        return Ok(());
    }
    if let Some(fetched) = &state.last_fetched {
        println!("{} (最終取得: {})",
            "最近の更新一覧".cyan().bold(), fetched.dimmed());
    } else {
        println!("{}", "最近の更新一覧".cyan().bold());
    }
    println!("{}", "─".repeat(60).dimmed());
    let display_count = count.unwrap_or_else(|| default_display_count(&state.entries));
    print_entries(&state.entries, display_count);
    Ok(())
}

fn cmd_config(install_dir: Option<String>) -> Result<()> {
    let mut config = load_config();
    if let Some(dir) = install_dir {
        let abs     = expand_path(&dir);
        let abs_str = abs.to_string_lossy().to_string();
        config.install_dir = Some(abs_str.clone());
        save_config(&config)?;
        println!("{} install_dir を設定しました: {}",
            "✓".green().bold(), abs_str.yellow());
    } else {
        println!("{}", "現在の設定".cyan().bold());
        println!("{}", "─".repeat(40).dimmed());
        println!("  install_dir : {}",
            config.install_dir.as_deref().unwrap_or("(未設定)").yellow());
        println!("  設定ファイル: {}",
            config_path().display().to_string().dimmed());
        println!("\n  設定変更: {}", "y2k config --install-dir <パス>".cyan());
    }
    Ok(())
}

fn cmd_clean() -> Result<()> {
    let path = state_path();
    if path.exists() {
        fs::remove_file(&path)?;
        println!("{} 状態ファイルを削除しました", "✓".green().bold());
    } else {
        println!("{}", "削除するファイルがありません".dimmed());
    }
    Ok(())
}

// ============================================================
// main
// ============================================================

fn main() {
    let cli = Cli::parse();
    let result = match &cli.command {
        Commands::Update  { count }       => cmd_update(*count),
        Commands::Upgrade { download }    => cmd_upgrade(*download),
        Commands::Install { kind }        => cmd_install(kind),
        Commands::List                    => cmd_list(),
        Commands::Show    { count }       => cmd_show(*count),
        Commands::Config  { install_dir } => cmd_config(install_dir.clone()),
        Commands::Clean                   => cmd_clean(),
    };
    if let Err(e) = result {
        eprintln!("{} {}", "エラー:".red().bold(), e);
        std::process::exit(1);
    }
}

// ============================================================
// テスト
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_temp_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("y2k-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_list_top_level_entries() {
        let dir = make_temp_test_dir("list-entries");
        fs::write(dir.join("a.txt"), "x").unwrap();
        fs::create_dir(dir.join("subdir")).unwrap();

        let entries = list_top_level_entries(&dir);
        assert_eq!(entries.len(), 2);
        assert!(entries.contains(&dir.join("a.txt")));
        assert!(entries.contains(&dir.join("subdir")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_top_level_entries_nonexistent_dir() {
        let dir = std::env::temp_dir().join("y2k-test-does-not-exist-xyz");
        let _ = fs::remove_dir_all(&dir);
        assert!(list_top_level_entries(&dir).is_empty());
    }

    #[test]
    fn test_diff_new_top_level_entries() {
        let dir = make_temp_test_dir("diff-entries");
        fs::write(dir.join("existing.txt"), "x").unwrap();
        let before = list_top_level_entries(&dir);

        fs::create_dir(dir.join("ゆめ2っきver0.129b")).unwrap();
        fs::write(dir.join("new_file.txt"), "y").unwrap();

        let new_entries = diff_new_top_level_entries(&dir, &before);
        assert_eq!(new_entries.len(), 2);
        assert!(new_entries.contains(&dir.join("ゆめ2っきver0.129b")));
        assert!(new_entries.contains(&dir.join("new_file.txt")));
        assert!(!new_entries.contains(&dir.join("existing.txt")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_remove_new_top_level_entries_keeps_existing_files() {
        let dir = make_temp_test_dir("remove-new-entries");
        fs::write(dir.join("keep_me.txt"), "important").unwrap();
        let before = list_top_level_entries(&dir);

        fs::create_dir(dir.join("ゆめ2っきver0.129b")).unwrap();
        fs::write(dir.join("ゆめ2っきver0.129b").join("inner.txt"), "z").unwrap();
        fs::write(dir.join("junk.tmp"), "garbage").unwrap();

        remove_new_top_level_entries(&dir, &before);

        assert!(!dir.join("ゆめ2っきver0.129b").exists());
        assert!(!dir.join("junk.tmp").exists());
        assert!(dir.join("keep_me.txt").exists());
        assert!(dir.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_remove_new_top_level_entries_dir_itself_never_touched() {
        let dir = make_temp_test_dir("remove-new-entries-self");
        fs::write(dir.join("project_file.rs"), "fn main() {}").unwrap();
        let before = list_top_level_entries(&dir);

        fs::create_dir(dir.join("extracted_version_dir")).unwrap();

        remove_new_top_level_entries(&dir, &before);

        assert!(dir.exists(), "dir自体は削除されてはならない");
        assert!(dir.join("project_file.rs").exists(), "既存ファイルは保持されるべき");
        assert!(!dir.join("extracted_version_dir").exists(), "新規ディレクトリは削除されるべき");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_extract_gdrive_id_file_d() {
        let url = "https://drive.google.com/file/d/1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgVE2upms/view";
        assert_eq!(
            extract_gdrive_id(url).as_deref(),
            Some("1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgVE2upms")
        );
    }

    #[test]
    fn test_extract_gdrive_id_uc() {
        let url = "https://drive.google.com/uc?id=1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgVE2upms";
        assert_eq!(
            extract_gdrive_id(url).as_deref(),
            Some("1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgVE2upms")
        );
    }

    #[test]
    fn test_extract_gdrive_id_none() {
        assert!(extract_gdrive_id("https://getuploader.com/test").is_none());
    }

    #[test]
    fn test_default_display_count_both() {
        let entries = vec![
            UpdateEntry { date: "2026/06/24".into(), kind: EntryKind::Patch,
                label: "パッチ1".into(), authors: vec![], note: None, dl_url: None },
            UpdateEntry { date: "2026/05/24".into(), kind: EntryKind::Core,
                label: "ver0.129b".into(), authors: vec![], note: None, dl_url: None },
            UpdateEntry { date: "2026/05/01".into(), kind: EntryKind::Patch,
                label: "パッチ0".into(), authors: vec![], note: None, dl_url: None },
        ];
        assert_eq!(default_display_count(&entries), 2);
    }

    #[test]
    fn test_build_viruscheck_url() {
        let html = r#"
            <input type="hidden" name="id" value="FILE123">
            <input type="hidden" name="authuser" value="0">
            <input type="hidden" name="confirm" value="t">
            <input type="hidden" name="uuid" value="abc-def-123">
        "#;
        let url = build_viruscheck_url(html, "FILE123").unwrap();
        assert!(url.contains("drive.usercontent.google.com"));
        assert!(url.contains("uuid=abc-def-123"));
        assert!(url.contains("confirm=t"));
        assert!(url.contains("id=FILE123"));
    }

    #[test]
    fn test_build_viruscheck_url_no_uuid() {
        let html = r#"<input type="hidden" name="id" value="ID">"#;
        assert!(build_viruscheck_url(html, "ID").is_none());
    }

    #[test]
    fn test_looks_like_filename_true() {
        assert!(looks_like_filename("game_v0.129b.7z"));
        assert!(looks_like_filename("ゆめ2っきver0.129b.zip"));
        assert!(looks_like_filename("archive.TAR.GZ"));
    }

    #[test]
    fn test_looks_like_filename_false() {
        assert!(!looks_like_filename("Google Drive - Virus scan warning"));
        assert!(!looks_like_filename("Google ドライブ"));
        assert!(!looks_like_filename(""));
    }

    #[test]
    fn test_gdrive_filename_from_confirm_page_uc_name_size() {
        let html = r#"<span class="uc-name-size"><a href="/open?id=ABC">ゆめ2っきver0.129b.7z</a> (4.0G)</span>"#;
        assert_eq!(
            gdrive_filename_from_confirm_page(html).as_deref(),
            Some("ゆめ2っきver0.129b.7z")
        );
    }

    #[test]
    fn test_gdrive_filename_from_confirm_page_uc_name_size_takes_priority() {
        let html = concat!(
            r#"<meta property="og:title" content="Google Drive - Virus scan warning">"#,
            r#"<span class="uc-name-size"><a href="/open?id=ABC">real_name.zip</a> (1.0G)</span>"#,
        );
        assert_eq!(
            gdrive_filename_from_confirm_page(html).as_deref(),
            Some("real_name.zip")
        );
    }

    #[test]
    fn test_gdrive_filename_from_confirm_page_uc_name_size_entities() {
        let html = r#"<span class="uc-name-size"><a href="/open?id=ABC">foo &amp; bar.zip</a> (1.0G)</span>"#;
        assert_eq!(
            gdrive_filename_from_confirm_page(html).as_deref(),
            Some("foo & bar.zip")
        );
    }

    #[test]
    fn test_gdrive_filename_from_og_prop_first() {
        let html = r#"<meta property="og:title" content="ゆめ2っきver0.129b.zip">"#;
        assert_eq!(gdrive_filename_from_confirm_page(html).as_deref(), Some("ゆめ2っきver0.129b.zip"));
    }

    #[test]
    fn test_gdrive_filename_from_og_content_first() {
        let html = r#"<meta content="ゆめ2っきver0.129b.zip" property="og:title">"#;
        assert_eq!(gdrive_filename_from_confirm_page(html).as_deref(), Some("ゆめ2っきver0.129b.zip"));
    }

    #[test]
    fn test_gdrive_filename_from_title_tag() {
        let html = "<title>ゆめ2っきver0.129b.zip - Google ドライブ</title>";
        assert_eq!(gdrive_filename_from_confirm_page(html).as_deref(), Some("ゆめ2っきver0.129b.zip"));
    }

    #[test]
    fn test_gdrive_filename_from_title_tag_en() {
        let html = "<title>game_v0.129b.7z - Google Drive</title>";
        assert_eq!(gdrive_filename_from_confirm_page(html).as_deref(), Some("game_v0.129b.7z"));
    }

    #[test]
    fn test_gdrive_filename_from_og_virus_scan_warning_rejected() {
        let html = r#"<meta property="og:title" content="Google Drive - Virus scan warning">"#;
        assert_eq!(gdrive_filename_from_confirm_page(html), None);
    }

    #[test]
    fn test_gdrive_filename_from_title_tag_virus_scan_warning_rejected() {
        let html = "<title>Google Drive - Virus scan warning</title>";
        assert_eq!(gdrive_filename_from_confirm_page(html), None);
    }

    #[test]
    fn test_get_url_from_gdrive_form() {
        let html = r#"<form action="https://drive.usercontent.google.com/download?id=ID&amp;confirm=t" method="get"></form>"#;
        let url = get_url_from_gdrive_form(html).unwrap();
        assert!(url.contains("confirm=t"));
        assert!(url.contains("ID"));
    }

    #[test]
    fn test_strip_tags() {
        assert_eq!(strip_tags("<b>hello</b> <i>world</i>"), "hello world");
    }

    #[test]
    fn test_extract_authors() {
        assert_eq!(
            extract_authors("山田氏 tanaka123氏"),
            vec!["山田", "tanaka123"]
        );
        assert_eq!(extract_authors("担当なし"), Vec::<String>::new());
    }

    #[test]
    fn test_duplicate_dedup() {
        let mut seen = std::collections::HashSet::new();
        let key = ("2026/06/24".to_string(), "パッチ11".to_string());
        assert!(seen.insert(key.clone()));
        assert!(!seen.insert(key));
    }
}
