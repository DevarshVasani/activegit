//! activegit benchmarks: measures the hot paths (fuzzy finder, word diff,
//! git status/diff, syntax highlighting, markdown preview) and writes a
//! self-contained HTML report plus console output.
//!
//! The UI modules are shared with the `activegit` binary via `#[path]`
//! includes so the benches always measure the real code.
//!
//! Usage:
//!   cargo run -p activegit-bench --release [-- --html target/bench-report.html]
//!
//! Flags:
//!   --html <path>     HTML report destination (default: target/bench-report.html)
//!   --json <path>     also write raw JSON samples next to the report
//!   --scale <factor>  multiply all iteration counts (default 1.0)
//!   --list            list benchmarks without running

// The UI modules are included wholesale; the bench only exercises a subset.
#![allow(dead_code)]

#[path = "../../git-tui/src/config.rs"]
mod config;
#[path = "../../git-tui/src/fuzzy.rs"]
mod fuzzy;
#[path = "../../git-tui/src/markdown.rs"]
mod markdown;
#[path = "../../git-tui/src/syntax.rs"]
mod syntax;
#[path = "../../git-tui/src/words.rs"]
mod words;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// fixture helpers (mirrors git-tui-core/src/testutil.rs, std-only + git2)
// ---------------------------------------------------------------------------

struct Fixture {
    _dir: tempfile::TempDir,
    repo: git2::Repository,
}

fn init_repo() -> Fixture {
    let dir = tempfile::TempDir::new().expect("create tempdir");
    let repo = git2::Repository::init(dir.path()).expect("init repo");
    repo.set_head("refs/heads/main").expect("set head to main");
    {
        let mut cfg = repo.config().expect("open config");
        cfg.set_str("user.name", "Bench").expect("user.name");
        cfg.set_str("user.email", "bench@example.com")
            .expect("user.email");
        cfg.set_str("commit.gpgsign", "false").ok();
    }
    Fixture { _dir: dir, repo }
}

fn write_workdir_file(repo: &git2::Repository, rel: &str, contents: &str) {
    let full = repo.workdir().expect("workdir").join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(&full, contents).expect("write file");
}

fn commit_all(repo: &git2::Repository, msg: &str) {
    let mut index = repo.index().expect("open index");
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .expect("stage all");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let sig = repo.signature().expect("signature");
    let parents: Vec<git2::Commit> = match repo.head() {
        Ok(head) if head.peel_to_commit().is_ok() => {
            vec![head.peel_to_commit().expect("head commit")]
        }
        _ => vec![],
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
        .expect("create commit");
}

fn commit_file(repo: &git2::Repository, path: &str, contents: &str, msg: &str) {
    write_workdir_file(repo, path, contents);
    let mut index = repo.index().expect("open index");
    index
        .add_path(Path::new(path))
        .unwrap_or_else(|_| panic!("stage {path}"));
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let sig = repo.signature().expect("signature");
    let parents: Vec<git2::Commit> = match repo.head() {
        Ok(head) if head.peel_to_commit().is_ok() => {
            vec![head.peel_to_commit().expect("head commit")]
        }
        _ => vec![],
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
        .expect("create commit");
}

// ---------------------------------------------------------------------------
// measurement
// ---------------------------------------------------------------------------

struct BenchResult {
    id: &'static str,
    title: &'static str,
    group: &'static str,
    detail: String,
    iters: usize,
    samples_ns: Vec<f64>,
}

impl BenchResult {
    fn sorted(&self) -> Vec<f64> {
        let mut v = self.samples_ns.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v
    }
    fn mean(&self) -> f64 {
        self.samples_ns.iter().sum::<f64>() / self.samples_ns.len() as f64
    }
    fn median(&self) -> f64 {
        let v = self.sorted();
        let n = v.len();
        if n % 2 == 1 {
            v[n / 2]
        } else {
            (v[n / 2 - 1] + v[n / 2]) / 2.0
        }
    }
    fn min(&self) -> f64 {
        self.samples_ns
            .iter()
            .cloned()
            .fold(f64::INFINITY, f64::min)
    }
    fn max(&self) -> f64 {
        self.samples_ns
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max)
    }
    fn stddev(&self) -> f64 {
        let m = self.mean();
        let var = self
            .samples_ns
            .iter()
            .map(|x| (x - m) * (x - m))
            .sum::<f64>()
            / self.samples_ns.len() as f64;
        var.sqrt()
    }
    fn ops_per_sec(&self) -> f64 {
        1e9 / self.mean()
    }
}

/// Warm up, then time `iters` single executions of `f`.
fn measure(
    id: &'static str,
    title: &'static str,
    group: &'static str,
    detail: String,
    iters: usize,
    mut f: impl FnMut(),
) -> BenchResult {
    for _ in 0..5.min(iters) {
        f();
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        f();
        samples.push(t.elapsed().as_nanos() as f64);
    }
    BenchResult {
        id,
        title,
        group,
        detail,
        iters,
        samples_ns: samples,
    }
}

fn scale_iters(base: usize, scale: f64) -> usize {
    ((base as f64 * scale).round() as usize).max(1)
}

// ---------------------------------------------------------------------------
// benchmark definitions
// ---------------------------------------------------------------------------

fn readme_like_doc() -> String {
    let mut s = String::from("# ActiveGit benchmark doc\n\n");
    s.push_str("Fast, keyboard-driven git TUI with **bold** and *italic* text.\n\n");
    s.push_str("## Features\n\n");
    for i in 0..30 {
        s.push_str(&format!(
            "- feature {i} with `inline code {i}` and more words\n"
        ));
    }
    s.push_str("\n## Tasks\n\n");
    for i in 0..10 {
        s.push_str(&format!("- [ ] todo item number {i}\n"));
    }
    s.push_str("\n```rust\nfn main() {\n    println!(\"hello { }\");\n}\n```\n\n");
    s.push_str("| name | value | note |\n|---|---|---|\n");
    for i in 0..15 {
        s.push_str(&format!(
            "| row-{i} | {n} | description text |\n",
            n = i * 7
        ));
    }
    s.push_str("\n> A blockquote with *emphasis*.\n\n---\n\nDone.\n");
    s
}

fn table_heavy_doc() -> String {
    let mut s = String::from("# Tables\n\n");
    for _ in 0..8 {
        s.push_str("| alpha | beta | gamma | delta |\n|:---|:---:|---:|---|\n");
        for r in 0..12 {
            s.push_str(&format!(
                "| cell-{r}-a | cell-{r}-b | {n} | tail |\n",
                n = r * 13
            ));
        }
        s.push('\n');
    }
    s
}

fn rust_lines(n: usize) -> Vec<String> {
    let pool = [
        "fn main() { let x = 1; }",
        "pub struct RepoStatus { pub branch: String, pub files: Vec<StatusEntry> }",
        "impl Renderer { fn flush_cur(&mut self) { self.lines.push(Line::from(\"\")); } }",
        "for (i, c) in cells.iter().enumerate() { widths[i] = widths[i].max(c.len()); }",
        "// A comment explaining the next tricky bit of borrow checking",
        "let diff = TextDiff::from_chars(old, new); // char-level diff",
        "match change.tag() { ChangeTag::Equal => keep(), _ => mark() }",
        "use ratatui::style::{Color as RatColor, Modifier};",
    ];
    (0..n)
        .map(|i| format!("{} // line {i}", pool[i % pool.len()]))
        .collect()
}

fn run_all(scale: f64) -> Vec<BenchResult> {
    let mut out = Vec::new();

    // ---- fuzzy ----
    out.push(measure(
        "fuzzy_score",
        "fuzzy_score single query",
        "fuzzy finder",
        "query \"ap\" vs candidate \"src/app.rs\"".into(),
        scale_iters(50_000, scale),
        || {
            std::hint::black_box(fuzzy::fuzzy_score("ap", "src/app.rs"));
        },
    ));

    let candidates: Vec<String> = (0..5000)
        .map(|i| format!("src/module_{i}/file_{i}.rs"))
        .collect();
    let refs: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
    out.push(measure(
        "fuzzy_rank_5k",
        "fuzzy rank over 5,000 files",
        "fuzzy finder",
        "query \"mod12\" over 5,000 generated paths".into(),
        scale_iters(200, scale),
        || {
            std::hint::black_box(fuzzy::rank("mod12", &refs));
        },
    ));

    // ---- word diff ----
    out.push(measure(
        "word_diff_short",
        "word_diff short lines",
        "diff highlight",
        "\"hello world\" vs \"hello WORLD\"".into(),
        scale_iters(20_000, scale),
        || {
            std::hint::black_box(words::word_diff("hello world", "hello WORLD"));
        },
    ));
    let long_old = "a".repeat(1000);
    let long_new = "b".repeat(1000);
    out.push(measure(
        "word_diff_capped",
        "word_diff overlong-line fast path",
        "diff highlight",
        "2x 1000-char lines (skips O(ND) char diff)".into(),
        scale_iters(5_000, scale),
        || {
            std::hint::black_box(words::word_diff(&long_old, &long_new));
        },
    ));

    // ---- git status (300 tracked files) ----
    let fx_status = init_repo();
    for i in 0..300 {
        write_workdir_file(
            &fx_status.repo,
            &format!("src/file_{i}.rs"),
            &format!("// file {i}\nfn f{i}() {{}}\n"),
        );
    }
    commit_all(&fx_status.repo, "add 300 files");
    for i in 0..30 {
        let full = fx_status
            .repo
            .workdir()
            .unwrap()
            .join(format!("src/file_{i}.rs"));
        let mut c = fs::read_to_string(&full).unwrap();
        c.push_str("// dirty\n");
        fs::write(&full, c).unwrap();
    }
    for i in 300..310 {
        write_workdir_file(
            &fx_status.repo,
            &format!("src/new_{i}.rs"),
            "// untracked\n",
        );
    }
    out.push(measure(
        "status_300_files",
        "repo_status with 300 tracked files",
        "git core",
        "300 tracked, 30 modified, 10 untracked".into(),
        scale_iters(30, scale),
        || {
            std::hint::black_box(
                git_tui_core::status::repo_status(&fx_status.repo).expect("status"),
            );
        },
    ));

    // ---- diffs on a 5k-line file ----
    let fx_diff = init_repo();
    let base: String = (1..=5000)
        .map(|i| format!("line {i} content here\n"))
        .collect();
    commit_file(&fx_diff.repo, "big.txt", &base, "init");
    let dirty = base
        .replacen("line 100 content here\n", "line 100 CHANGED here\n", 1)
        .replacen("line 2500 content here\n", "line 2500 CHANGED here\n", 1)
        .replacen("line 4900 content here\n", "line 4900 CHANGED here\n", 1);
    fs::write(fx_diff.repo.workdir().unwrap().join("big.txt"), &dirty).unwrap();
    out.push(measure(
        "unstaged_diff_5k",
        "unstaged_diff on 5,000-line file",
        "git core",
        "3 scattered single-line changes, 3 context lines".into(),
        scale_iters(30, scale),
        || {
            std::hint::black_box(
                git_tui_core::diff::unstaged_diff(&fx_diff.repo, "big.txt").expect("diff"),
            );
        },
    ));
    out.push(measure(
        "whole_file_5k",
        "whole_file_diff on 5,000-line file",
        "git core",
        "clean-file whole-file view, all-context hunk".into(),
        scale_iters(100, scale),
        || {
            std::hint::black_box(
                git_tui_core::diff::whole_file_diff(&fx_diff.repo, "big.txt").expect("whole"),
            );
        },
    ));

    // ---- staged diff ----
    let fx_staged = init_repo();
    commit_file(&fx_staged.repo, "a.txt", "one\ntwo\nthree\n", "init");
    write_workdir_file(&fx_staged.repo, "a.txt", "one\nTWO\nthree\nfour\n");
    {
        let mut index = fx_staged.repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
    }
    out.push(measure(
        "staged_diff_small",
        "staged_diff small file",
        "git core",
        "index vs HEAD, one modified hunk".into(),
        scale_iters(100, scale),
        || {
            std::hint::black_box(
                git_tui_core::diff::staged_diff(&fx_staged.repo, "a.txt").expect("staged"),
            );
        },
    ));

    // ---- syntax highlighting ----
    let theme = config::Theme::tokyo_night();
    out.push(measure(
        "highlight_line_cached",
        "highlight_line cache hit",
        "syntax",
        "same Rust line every iteration (HashMap hit)".into(),
        scale_iters(20_000, scale),
        || {
            std::hint::black_box(syntax::highlight_line(
                "src/main.rs",
                "fn main() { let x = 1; }",
                theme,
            ));
        },
    ));
    let lines = rust_lines(50);
    let mut k = 0usize;
    out.push(measure(
        "highlight_line_mixed",
        "highlight_line 50-line rotation",
        "syntax",
        "cycles 50 distinct Rust lines (mixed hit/miss)".into(),
        scale_iters(3_000, scale),
        || {
            let l = &lines[k % lines.len()];
            k += 1;
            std::hint::black_box(syntax::highlight_line("src/main.rs", l, theme));
        },
    ));
    let file_lines = rust_lines(200);
    let file_refs: Vec<&str> = file_lines.iter().map(|s| s.as_str()).collect();
    out.push(measure(
        "highlight_file_200",
        "highlight_file_lines x200 Rust lines",
        "syntax",
        "whole-file view with shared parser state".into(),
        scale_iters(100, scale),
        || {
            std::hint::black_box(syntax::highlight_file_lines(
                "src/main.rs",
                &file_refs,
                theme,
            ));
        },
    ));

    // ---- markdown ----
    let doc = readme_like_doc();
    out.push(measure(
        "markdown_readme_like",
        "render_markdown doc page",
        "markdown",
        format!("{} bytes: headings, lists, tasks, code, table", doc.len()),
        scale_iters(200, scale),
        || {
            std::hint::black_box(markdown::render_markdown(&doc, theme, 80));
        },
    ));
    let tables = table_heavy_doc();
    out.push(measure(
        "markdown_tables",
        "render_markdown table-heavy",
        "markdown",
        format!("{} bytes: 8 tables x 12 rows", tables.len()),
        scale_iters(200, scale),
        || {
            std::hint::black_box(markdown::render_markdown(&tables, theme, 80));
        },
    ));

    out
}

// ---------------------------------------------------------------------------
// formatting + report
// ---------------------------------------------------------------------------

fn fmt_time(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{ns:.1} ns")
    } else if ns < 1_000_000.0 {
        format!("{:.2} µs", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else {
        format!("{:.2} s", ns / 1_000_000_000.0)
    }
}

fn fmt_num(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.2}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else {
        format!("{n:.1}")
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn cmd_output(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn utc_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // days since epoch -> civil date (Howard Hinnant's algorithm)
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    y += if m <= 2 { 1 } else { 0 };
    let (hh, mm, ss) = (secs % 86_400 / 3_600, secs % 3_600 / 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02} UTC")
}

fn histogram_svg(samples: &[f64], w: usize) -> String {
    const BINS: usize = 24;
    let min = samples.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = samples.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).max(f64::EPSILON);
    let mut bins = [0usize; BINS];
    for s in samples {
        let mut b = ((s - min) / span * BINS as f64) as usize;
        if b >= BINS {
            b = BINS - 1;
        }
        bins[b] += 1;
    }
    let peak = *bins.iter().max().unwrap_or(&1).max(&1);
    let mut bars = String::new();
    for (i, b) in bins.iter().enumerate() {
        let h = (*b as f64 / peak as f64 * 100.0).max(if *b > 0 { 4.0 } else { 0.0 });
        let lo = min + span * i as f64 / BINS as f64;
        bars.push_str(&format!(
            "<div class=\"hbin\" style=\"height:{h:.1}%\" title=\"≥ {} ({} samples)\"></div>",
            fmt_time(lo),
            b
        ));
    }
    format!(
        "<div class=\"hist\" style=\"max-width:{w}px\">{bars}<div class=\"hcap\"><span>{}</span><span>{}</span></div></div>",
        fmt_time(min),
        fmt_time(max)
    )
}

fn build_html(results: &[BenchResult]) -> String {
    let ts = utc_timestamp();
    let rustc = cmd_output("rustc", &["--version"]);
    let git_rev = cmd_output("git", &["rev-parse", "--short", "HEAD"]);
    let cpu = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "?".into());
    let os = format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH);
    let max_median = results
        .iter()
        .map(|r| r.median())
        .fold(0.0f64, f64::max)
        .max(1.0);

    let mut rows = String::new();
    for r in results {
        let pct = (r.median() / max_median * 100.0).clamp(1.0, 100.0);
        rows.push_str(&format!(
            "<tr><td><a href=\"#{id}\">{title}</a><div class=\"grp\">{group}</div></td>\
             <td class=\"num\">{iters}</td><td class=\"num\">{mean}</td>\
             <td class=\"num\"><b>{median}</b></td>\
             <td class=\"num\">{min} – {max}</td><td class=\"num\">± {sd}</td>\
             <td class=\"num\">{ops}/s</td>\
             <td><div class=\"bar\"><div class=\"fill\" style=\"width:{pct:.1}%\"></div></div></td></tr>\n",
            id = r.id,
            title = html_escape(r.title),
            group = html_escape(r.group),
            iters = r.iters,
            mean = fmt_time(r.mean()),
            median = fmt_time(r.median()),
            min = fmt_time(r.min()),
            max = fmt_time(r.max()),
            sd = fmt_time(r.stddev()),
            ops = fmt_num(r.ops_per_sec()),
        ));
    }

    let mut cards = String::new();
    for r in results {
        cards.push_str(&format!(
            "<section class=\"card\" id=\"{id}\">\
             <h3>{title} <code>{id}</code></h3>\
             <p class=\"detail\">{group} · {detail} · {iters} iterations</p>\
             <div class=\"stats\">\
             <div><span>mean</span><b>{mean}</b></div>\
             <div><span>median</span><b>{median}</b></div>\
             <div><span>min</span><b>{min}</b></div>\
             <div><span>max</span><b>{max}</b></div>\
             <div><span>stddev</span><b>{sd}</b></div>\
             <div><span>throughput</span><b>{ops}/s</b></div>\
             </div>{hist}</section>\n",
            id = r.id,
            title = html_escape(r.title),
            group = html_escape(r.group),
            detail = html_escape(&r.detail),
            iters = r.iters,
            mean = fmt_time(r.mean()),
            median = fmt_time(r.median()),
            min = fmt_time(r.min()),
            max = fmt_time(r.max()),
            sd = fmt_time(r.stddev()),
            ops = fmt_num(r.ops_per_sec()),
            hist = histogram_svg(&r.samples_ns, 560),
        ));
    }

    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ActiveGit benchmarks</title>
<style>
:root{{--bg:#1a1b26;--panel:#24283b;--card:#2a2e44;--fg:#c0caf5;--dim:#787c99;
--blue:#7aa2f7;--green:#9ece6a;--cyan:#7dcfff;--border:#3b3f5c}}
*{{box-sizing:border-box}}body{{background:var(--bg);color:var(--fg);
font:15px/1.55 -apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;
margin:0;padding:0 0 64px}}header{{padding:36px 28px 20px;max-width:1080px;margin:0 auto}}
h1{{margin:0 0 4px;font-size:30px}}h1 span{{color:var(--green)}}
.sub{{color:var(--dim);margin:0 0 14px}}
.meta{{display:flex;flex-wrap:wrap;gap:8px}}.meta div{{background:var(--panel);
border:1px solid var(--border);border-radius:8px;padding:6px 12px;font-size:13px}}
.meta b{{color:var(--cyan);font-weight:600}}main{{max-width:1080px;margin:0 auto;padding:0 28px}}
table{{width:100%;border-collapse:collapse;background:var(--panel);
border:1px solid var(--border);border-radius:12px;overflow:hidden}}
th,td{{text-align:left;padding:10px 12px;border-bottom:1px solid var(--border);font-size:14px}}
th{{color:var(--dim);text-transform:uppercase;font-size:12px;letter-spacing:.04em}}
tr:last-child td{{border-bottom:none}}td.num,th.num{{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}}
.grp{{color:var(--dim);font-size:12px}}a{{color:var(--blue);text-decoration:none}}a:hover{{text-decoration:underline}}
.bar{{width:140px;height:8px;background:var(--bg);border-radius:4px;overflow:hidden}}
.fill{{height:100%;background:linear-gradient(90deg,var(--green),var(--cyan))}}
.card{{background:var(--panel);border:1px solid var(--border);border-radius:12px;
padding:18px 20px;margin:18px 0}}.card h3{{margin:0 0 2px;font-size:17px}}
.card code{{color:var(--dim);font-size:12px}}.detail{{color:var(--dim);font-size:13px;margin:2px 0 12px}}
.stats{{display:grid;grid-template-columns:repeat(auto-fit,minmax(130px,1fr));gap:8px;margin-bottom:12px}}
.stats div{{background:var(--card);border:1px solid var(--border);border-radius:8px;padding:8px 10px}}
.stats span{{display:block;color:var(--dim);font-size:11px;text-transform:uppercase;letter-spacing:.05em}}
.stats b{{font-size:15px;font-variant-numeric:tabular-nums}}
.hist{{display:flex;align-items:flex-end;gap:2px;height:90px;background:var(--card);
border:1px solid var(--border);border-radius:8px;padding:10px 10px 22px;position:relative}}
.hbin{{flex:1;background:var(--blue);border-radius:2px 2px 0 0;min-height:2px;opacity:.85}}
.hcap{{position:absolute;left:10px;right:10px;bottom:4px;display:flex;justify-content:space-between;
color:var(--dim);font-size:11px}}footer{{max-width:1080px;margin:22px auto 0;padding:0 28px;color:var(--dim);font-size:13px}}
</style></head><body>
<header><h1>ActiveGit <span>benchmarks</span></h1>
<p class="sub">{n} benchmarks · {ts} · release profile (LTO off for bench speed, opt-level 3)</p>
<div class="meta"><div><b>rustc</b> {rustc}</div><div><b>rev</b> {rev}</div>
<div><b>os</b> {os}</div><div><b>cpus</b> {cpu}</div></div></header>
<main><h2>Summary (sorted by suite order, bars = median vs slowest)</h2>
<table><thead><tr><th>Benchmark</th><th class="num">Iters</th><th class="num">Mean</th>
<th class="num">Median</th><th class="num">Min – max</th><th class="num">Stddev</th>
<th class="num">Throughput</th><th>Relative</th></tr></thead><tbody>
{rows}</tbody></table>
<h2>Details &amp; latency distribution</h2>
{cards}</main>
<footer><p>Methodology: 5 warmup iterations, then N timed single executions of the
operation only (fixtures are built once beforehand). Times are wall-clock per
operation on this machine; distributions show all samples. Re-run with
<code>cargo run -p activegit-bench --release</code>.</p></footer>
</body></html>"#,
        n = results.len(),
        ts = html_escape(&ts),
        rustc = html_escape(&rustc),
        rev = html_escape(&git_rev),
        os = html_escape(&os),
        cpu = html_escape(&cpu),
        rows = rows,
        cards = cards,
    )
}

fn print_table(results: &[BenchResult]) {
    println!(
        "{:<22} {:>8} {:>12} {:>12} {:>12} {:>12}",
        "benchmark", "iters", "mean", "median", "min", "max"
    );
    println!("{}", "-".repeat(84));
    for r in results {
        println!(
            "{:<22} {:>8} {:>12} {:>12} {:>12} {:>12}  {:>10}/s",
            r.id,
            r.iters,
            fmt_time(r.mean()),
            fmt_time(r.median()),
            fmt_time(r.min()),
            fmt_time(r.max()),
            fmt_num(r.ops_per_sec()),
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usage: activegit-bench [--html PATH] [--json PATH] [--scale F] [--list]");
        return;
    }
    let mut html = String::from("target/bench-report.html");
    let mut json: Option<String> = None;
    let mut scale = 1.0;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--html" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    html = v.clone();
                }
            }
            "--json" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    json = Some(v.clone());
                }
            }
            "--scale" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    scale = v.parse().unwrap_or(1.0);
                }
            }
            "--list" => {
                // identifiers are stable; list without running:
                for id in [
                    "fuzzy_score",
                    "fuzzy_rank_5k",
                    "word_diff_short",
                    "word_diff_capped",
                    "status_300_files",
                    "unstaged_diff_5k",
                    "whole_file_5k",
                    "staged_diff_small",
                    "highlight_line_cached",
                    "highlight_line_mixed",
                    "highlight_file_200",
                    "markdown_readme_like",
                    "markdown_tables",
                ] {
                    println!("{id}");
                }
                return;
            }
            _ => {}
        }
        i += 1;
    }

    println!("activegit benchmarks (release, scale={scale}) …");
    let t0 = Instant::now();
    let results = run_all(scale);
    println!(
        "measured {} benches in {:.1}s\n",
        results.len(),
        t0.elapsed().as_secs_f64()
    );
    print_table(&results);

    let page = build_html(&results);
    let path = PathBuf::from(&html);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).expect("create report dir");
        }
    }
    fs::write(&path, &page).expect("write html report");
    println!("\nHTML report: {}", path.display());

    if let Some(jp) = json {
        let mut s = String::from("{\"benchmarks\":[");
        for (k, r) in results.iter().enumerate() {
            if k > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"id\":\"{}\",\"mean_ns\":{},\"median_ns\":{},\"min_ns\":{},\"max_ns\":{},\"stddev_ns\":{},\"iters\":{}}}",
                r.id,
                r.mean(),
                r.median(),
                r.min(),
                r.max(),
                r.stddev(),
                r.iters
            ));
        }
        s.push_str("]}");
        fs::write(&jp, &s).expect("write json");
        println!("JSON data:  {jp}");
    }
}
