use anyhow::{Context, Result};
use clap::Parser;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Parser, Debug)]
#[command(name = "notfail")]
#[command(about = "Run commands and capture output without the spam")]
struct Args {
    /// The command to run
    #[arg(short = 'c', long, default_value = "npm test -- --silent")]
    cmd: String,

    /// Output to a file
    #[arg(short, long)]
    file: Option<PathBuf>,

    /// Do not run as a singleton (allow multiple instances)
    #[arg(short, long)]
    multi: bool,

    /// Restart if already running
    #[arg(short, long)]
    restart: bool,
}

#[derive(Serialize, Deserialize, Debug)]
enum Message {
    Output(String),
    Progress { current: u32, total: u32 },
    Exit(i32),
    Restart,
    Ping,
    Pong,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
struct DirEntry {
    test_count: u32,
    last_run: u64, // Unix timestamp
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct TestCache {
    directories: HashMap<String, DirEntry>,
}

impl TestCache {
    fn load() -> Self {
        let path = Self::cache_path();
        if let Ok(content) = fs::read_to_string(&path) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    fn save(&self) {
        let path = Self::cache_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(content) = serde_json::to_string(self) {
            let _ = fs::write(path, content);
        }
    }

    fn cache_path() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("notfail")
            .join("test_cache.json")
    }

    fn get_expected_tests(&self, dir: &str) -> Option<u32> {
        self.directories.get(dir).map(|e| e.test_count)
    }

    fn set_expected_tests(&mut self, dir: &str, count: u32) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        self.directories.insert(
            dir.to_string(),
            DirEntry {
                test_count: count,
                last_run: now,
            },
        );
        self.save();
    }

    fn touch(&mut self, dir: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if let Some(entry) = self.directories.get_mut(dir) {
            entry.last_run = now;
        } else {
            self.directories.insert(
                dir.to_string(),
                DirEntry {
                    test_count: 0,
                    last_run: now,
                },
            );
        }
        self.save();
    }

    fn get_recent_directories(&self, limit: usize) -> Vec<String> {
        let mut entries: Vec<_> = self.directories.iter().collect();
        entries.sort_by(|a, b| b.1.last_run.cmp(&a.1.last_run));
        entries
            .into_iter()
            .take(limit)
            .map(|(k, _)| k.clone())
            .collect()
    }
}

/// Strip ANSI escape codes from a string
fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // ESC character - start of ANSI sequence
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                // Skip until we hit a letter (the command terminator)
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

fn is_failure_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let upper = trimmed.to_uppercase();

    // Detect failure indicators
    trimmed.starts_with("FAIL")
        || trimmed.starts_with("✗")
        || trimmed.starts_with("×")
        || trimmed.starts_with("Error:")
        || trimmed.starts_with("error:")
        || upper.starts_with("ERROR")
        || trimmed.contains("AssertionError")
        || trimmed.contains("Expected:")
        || trimmed.contains("Received:")
        || trimmed.contains("- Expected")
        || trimmed.contains("+ Received")
        || (trimmed.starts_with("at ") && trimmed.contains(":")) // stack trace
        || trimmed.starts_with("●") // Jest failure marker
}

// Parse test count from summary line like "Tests 2 failed | 1915 passed | 70 skipped (1987)"
fn parse_test_summary(line: &str) -> Option<(u32, u32)> {
    // Strip ANSI codes first, since vitest adds color
    let clean = strip_ansi_codes(line);
    let trimmed = clean.trim();

    // Look for the total in parentheses at the end
    let paren_start = trimmed.rfind('(')?;
    let paren_end = trimmed.rfind(')')?;
    let Ok(total) = trimmed[paren_start + 1..paren_end].trim().parse::<u32>() else {
        return None;
    };

    // Count completed tests (failed + passed + skipped)
    let mut completed = 0u32;

    for keyword in ["failed", "passed", "skipped"] {
        if let Some(idx) = trimmed.find(keyword) {
            let before = &trimmed[..idx];
            if let Some(num_str) = before.split_whitespace().last()
                && let Ok(n) = num_str.parse::<u32>()
            {
                completed += n;
            }
        }
    }

    if completed > 0 {
        Some((completed, total))
    } else {
        None
    }
}

// Parse running test count from lines like " ✓  jsdom  src/test.ts (8 tests) 2ms"
// Returns the number of tests in this single file
fn parse_completed_file_tests(line: &str) -> Option<u32> {
    // Strip ANSI codes first, since vitest wraps ✓ in color codes
    let clean = strip_ansi_codes(line);
    let trimmed = clean.trim();

    // Only match completed test lines (with ✓)
    if !trimmed.starts_with("✓") {
        return None;
    }

    // Look for "(N tests)" or "(N test)" pattern
    if let Some(paren_start) = trimmed.rfind('(')
        && let Some(paren_end) = trimmed.rfind(')')
    {
        let inner = &trimmed[paren_start + 1..paren_end];
        if inner.contains("test")
            && let Some(num_str) = inner.split_whitespace().next()
            && let Ok(n) = num_str.parse::<u32>()
        {
            return Some(n);
        }
    }

    None
}

fn render_progress_bar(current: u32, total: u32, width: usize) -> String {
    let percentage = if total > 0 {
        (current as f64 / total as f64 * 100.0).min(100.0)
    } else {
        0.0
    };

    let filled = ((percentage / 100.0) * width as f64) as usize;
    let empty = width.saturating_sub(filled);

    let bar: String = "█".repeat(filled) + &"░".repeat(empty);

    format!("\r[{}] {:>3.0}% ({}/{})", bar, percentage, current, total)
}

fn get_socket_path(command: &str) -> PathBuf {
    let hash = command
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let runtime_dir = dirs::runtime_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    runtime_dir.join(format!("notfail-{:x}.sock", hash))
}

fn is_server_running(socket_path: &PathBuf) -> bool {
    if let Ok(mut stream) = UnixStream::connect(socket_path) {
        let msg = serde_json::to_string(&Message::Ping).unwrap();
        if stream.write_all(msg.as_bytes()).is_ok()
            && stream.write_all(b"\n").is_ok()
            && stream.flush().is_ok()
        {
            let mut reader = BufReader::new(&stream);
            let mut response = String::new();
            if reader.read_line(&mut response).is_ok()
                && let Ok(Message::Pong) = serde_json::from_str(&response)
            {
                return true;
            }
        }
    }
    false
}

fn send_restart(socket_path: &PathBuf) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)?;
    let msg = serde_json::to_string(&Message::Restart)?;
    stream.write_all(msg.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn attach_to_server(socket_path: &PathBuf) -> Result<i32> {
    let stream =
        UnixStream::connect(socket_path).context("Failed to connect to running instance")?;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Message>(&line)? {
            Message::Output(text) => {
                print!("{}", text);
                std::io::stdout().flush()?;
            }
            Message::Progress { current, total } => {
                if total > 0 {
                    print!("{}", render_progress_bar(current, total, 40));
                } else if current > 0 {
                    print!("\r{} tests passed", current);
                }
                std::io::stdout().flush()?;
            }
            Message::Exit(code) => {
                // Clear progress bar line
                print!("\r{}\r", " ".repeat(60));
                std::io::stdout().flush()?;
                return Ok(code);
            }
            _ => {}
        }
    }
    Ok(0)
}

struct Server {
    clients: Arc<Mutex<Vec<UnixStream>>>,
    file_output: Option<Arc<Mutex<File>>>,
    should_restart: Arc<Mutex<bool>>,
    failure_buffer: Arc<Mutex<Vec<String>>>,
    test_count: Arc<Mutex<u32>>,
    final_total: Arc<Mutex<Option<u32>>>,
}

impl Server {
    fn new(file_path: Option<PathBuf>) -> Result<Self> {
        let file_output = if let Some(path) = file_path {
            Some(Arc::new(Mutex::new(
                File::create(&path).context("Failed to create output file")?,
            )))
        } else {
            None
        };

        Ok(Self {
            clients: Arc::new(Mutex::new(Vec::new())),
            file_output,
            should_restart: Arc::new(Mutex::new(false)),
            failure_buffer: Arc::new(Mutex::new(Vec::new())),
            test_count: Arc::new(Mutex::new(0)),
            final_total: Arc::new(Mutex::new(None)),
        })
    }

    fn broadcast(&self, msg: &Message) {
        let json = match serde_json::to_string(msg) {
            Ok(j) => j,
            Err(_) => return,
        };

        let mut clients = self.clients.lock().unwrap();
        clients.retain_mut(|client| {
            client
                .write_all(json.as_bytes())
                .and_then(|_| client.write_all(b"\n"))
                .and_then(|_| client.flush())
                .is_ok()
        });
    }

    fn run_command(
        &self,
        command: &str,
        expected_tests: Option<u32>,
        working_dir: &str,
    ) -> Result<i32> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to open pty")?;

        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg(command);

        // Set working directory to current directory
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .context("Failed to spawn command")?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("Failed to clone reader")?;
        let file_output = self.file_output.clone();
        let failure_buffer = Arc::clone(&self.failure_buffer);
        let test_count = Arc::clone(&self.test_count);
        let final_total = Arc::clone(&self.final_total);
        let clients = Arc::clone(&self.clients);
        let working_dir = working_dir.to_string();
        let cached_count = expected_tests.unwrap_or(0);

        let output_handle = thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            let mut line_buffer = String::new();
            let mut in_failure_block = false;
            let mut failure_block_lines = 0;
            let mut current_tests: u32 = 0;
            let mut last_progress_update = std::time::Instant::now();
            const MAX_FAILURE_LINES: usize = 50;

            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buffer[..n]);

                        for ch in chunk.chars() {
                            if ch == '\n' {
                                line_buffer.push('\n');

                                // Check for test summary line
                                if (line_buffer.trim().starts_with("Tests")
                                    || line_buffer.contains("passed"))
                                    && let Some((completed, total)) =
                                        parse_test_summary(&line_buffer)
                                {
                                    current_tests = completed;
                                    *test_count.lock().unwrap() = completed;
                                    *final_total.lock().unwrap() = Some(total);
                                }

                                // Check for completed file tests (adds to running total)
                                if let Some(count) = parse_completed_file_tests(&line_buffer) {
                                    current_tests += count;
                                    *test_count.lock().unwrap() = current_tests;
                                }

                                // Update progress bar periodically
                                if last_progress_update.elapsed().as_millis() > 100 {
                                    let current = *test_count.lock().unwrap();
                                    // Use cached expected, or final_total if we've seen the summary
                                    let total = expected_tests
                                        .or(*final_total.lock().unwrap())
                                        .unwrap_or(0);

                                    // Save to cache if current count exceeds cached count
                                    // This ensures we have a reasonable estimate even if interrupted
                                    if current > cached_count {
                                        let mut cache = TestCache::load();
                                        cache.set_expected_tests(&working_dir, current);
                                    }

                                    // Show progress - either as bar (if we know total) or just count
                                    if total > 0 {
                                        print!("{}", render_progress_bar(current, total, 40));
                                    } else if current > 0 {
                                        print!("\r{} tests passed", current);
                                    }
                                    let _ = std::io::stdout().flush();

                                    // Broadcast progress to attached clients
                                    let json = serde_json::to_string(&Message::Progress {
                                        current,
                                        total,
                                    })
                                    .unwrap();
                                    let mut clients = clients.lock().unwrap();
                                    clients.retain_mut(|client| {
                                        client
                                            .write_all(json.as_bytes())
                                            .and_then(|_| client.write_all(b"\n"))
                                            .and_then(|_| client.flush())
                                            .is_ok()
                                    });

                                    last_progress_update = std::time::Instant::now();
                                }

                                // Check if this starts a failure block
                                if is_failure_line(&line_buffer) {
                                    in_failure_block = true;
                                    failure_block_lines = 0;
                                }

                                // Capture failure output
                                if in_failure_block {
                                    if let Ok(mut fb) = failure_buffer.lock() {
                                        fb.push(line_buffer.clone());
                                        if fb.len() > 1000 {
                                            fb.drain(0..100);
                                        }
                                    }
                                    failure_block_lines += 1;
                                    if failure_block_lines > MAX_FAILURE_LINES
                                        && !is_failure_line(&line_buffer)
                                    {
                                        in_failure_block = false;
                                    }
                                }

                                // Write to file if specified
                                if let Some(ref file) = file_output
                                    && let Ok(mut f) = file.lock()
                                {
                                    let _ = f.write_all(line_buffer.as_bytes());
                                }

                                line_buffer.clear();
                            } else {
                                line_buffer.push(ch);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }

            // Flush any remaining content
            if !line_buffer.is_empty()
                && let Some(ref file) = file_output
                && let Ok(mut f) = file.lock()
            {
                let _ = f.write_all(line_buffer.as_bytes());
            }
        });

        let status = child.wait().context("Failed to wait for child")?;
        drop(pair.master);
        output_handle.join().unwrap();

        // Clear progress bar line
        print!("\r{}\r", " ".repeat(60));
        let _ = std::io::stdout().flush();

        let exit_code = status.exit_code() as i32;

        // Print captured failures at the end
        let failures = self.failure_buffer.lock().unwrap();
        if !failures.is_empty() {
            for line in failures.iter() {
                print!("{}", line);
            }
            let _ = std::io::stdout().flush();

            // Also send to clients
            let mut clients = self.clients.lock().unwrap();
            for line in failures.iter() {
                let json = serde_json::to_string(&Message::Output(line.clone())).unwrap();
                for client in clients.iter_mut() {
                    let _ = client.write_all(json.as_bytes());
                    let _ = client.write_all(b"\n");
                }
            }
            for client in clients.iter_mut() {
                let _ = client.flush();
            }
        } else if exit_code == 0 {
            let msg = "All tests passed!\n";
            print!("{}", msg);
            let _ = std::io::stdout().flush();

            let json = serde_json::to_string(&Message::Output(msg.to_string())).unwrap();
            let mut clients = self.clients.lock().unwrap();
            for client in clients.iter_mut() {
                let _ = client.write_all(json.as_bytes());
                let _ = client.write_all(b"\n");
                let _ = client.flush();
            }
        }

        self.broadcast(&Message::Exit(exit_code));

        Ok(exit_code)
    }

    fn get_final_total(&self) -> Option<u32> {
        *self.final_total.lock().unwrap()
    }
}

fn run_server(socket_path: PathBuf, command: String, file_path: Option<PathBuf>) -> Result<i32> {
    // Remove old socket if exists
    let _ = fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path).context("Failed to bind socket")?;
    listener
        .set_nonblocking(true)
        .context("Failed to set non-blocking")?;

    // Load test cache
    let cache = TestCache::load();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let expected_tests = cache.get_expected_tests(&cwd);

    let server = Arc::new(Server::new(file_path)?);
    let server_for_command = Arc::clone(&server);
    let should_restart = Arc::clone(&server.should_restart);

    // Spawn thread to accept connections
    let clients = Arc::clone(&server.clients);
    let should_restart_listener = Arc::clone(&should_restart);
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(mut client) => {
                    let mut reader = BufReader::new(client.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_ok() {
                        match serde_json::from_str::<Message>(&line) {
                            Ok(Message::Ping) => {
                                let response = serde_json::to_string(&Message::Pong).unwrap();
                                let _ = client.write_all(response.as_bytes());
                                let _ = client.write_all(b"\n");
                                let _ = client.flush();
                            }
                            Ok(Message::Restart) => {
                                *should_restart_listener.lock().unwrap() = true;
                            }
                            _ => {
                                clients.lock().unwrap().push(client);
                            }
                        }
                    } else {
                        clients.lock().unwrap().push(client);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
    });

    // Run the command
    let exit_code = server_for_command.run_command(&command, expected_tests, &cwd)?;

    // Update cache with actual test count
    if let Some(total) = server_for_command.get_final_total() {
        let mut cache = TestCache::load();
        cache.set_expected_tests(&cwd, total);
    }

    // Give clients a moment to receive the exit message
    thread::sleep(std::time::Duration::from_millis(100));

    // Clean up socket
    let _ = fs::remove_file(&socket_path);

    // Check if we should restart
    if *should_restart.lock().unwrap() {
        return run_server(socket_path, command, None);
    }

    Ok(exit_code)
}

fn check_package_json_or_select_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;

    // Check if package.json exists in current directory
    if cwd.join("package.json").exists() {
        return Ok(cwd);
    }

    // No package.json, offer recent directories
    let cache = TestCache::load();
    let recent = cache.get_recent_directories(10);

    if recent.is_empty() {
        anyhow::bail!("No package.json found and no recent test directories cached");
    }

    // Filter to only directories that still have package.json
    let valid_dirs: Vec<_> = recent
        .into_iter()
        .filter(|d| PathBuf::from(d).join("package.json").exists())
        .collect();

    if valid_dirs.is_empty() {
        anyhow::bail!("No package.json found and no valid cached directories");
    }

    println!("No package.json found. Select a recent directory:");
    for (i, dir) in valid_dirs.iter().enumerate() {
        println!("{}> {}", i, dir);
    }
    print!("\nPress 0-{}: ", valid_dirs.len().saturating_sub(1));
    std::io::stdout().flush()?;

    // Read single character input
    let selection = read_single_digit(valid_dirs.len())?;

    println!(); // Newline after selection
    Ok(PathBuf::from(&valid_dirs[selection]))
}

fn read_single_digit(max: usize) -> Result<usize> {
    use std::io::Read;

    // Set terminal to raw mode to read single keypress
    let mut termios = std::mem::MaybeUninit::uninit();
    let fd = 0; // stdin

    unsafe {
        if libc::tcgetattr(fd, termios.as_mut_ptr()) != 0 {
            anyhow::bail!("Failed to get terminal attributes");
        }
    }

    let mut termios = unsafe { termios.assume_init() };
    let old_termios = termios;

    // Disable canonical mode and echo
    termios.c_lflag &= !(libc::ICANON | libc::ECHO);
    termios.c_cc[libc::VMIN] = 1;
    termios.c_cc[libc::VTIME] = 0;

    unsafe {
        if libc::tcsetattr(fd, libc::TCSANOW, &termios) != 0 {
            anyhow::bail!("Failed to set terminal attributes");
        }
    }

    // Read single byte
    let mut buf = [0u8; 1];
    let result = std::io::stdin().read_exact(&mut buf);

    // Restore terminal
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &old_termios);
    }

    result?;

    let ch = buf[0] as char;
    if let Some(digit) = ch.to_digit(10) {
        let idx = digit as usize;
        if idx < max {
            return Ok(idx);
        }
    }

    anyhow::bail!("Invalid selection: {}", ch)
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Check for package.json or select from recent directories
    let working_dir = check_package_json_or_select_dir()?;
    std::env::set_current_dir(&working_dir)?;

    let socket_path = get_socket_path(&args.cmd);

    if args.multi {
        // Multi mode: just run the command directly without singleton logic
        let mut cache = TestCache::load();
        let cwd = working_dir.to_string_lossy().to_string();
        cache.touch(&cwd);
        let expected_tests = cache.get_expected_tests(&cwd);

        let server = Server::new(args.file)?;
        let exit_code = server.run_command(&args.cmd, expected_tests, &cwd)?;

        // Update cache
        if let Some(total) = server.get_final_total() {
            let mut cache = TestCache::load();
            cache.set_expected_tests(&cwd, total);
        }

        std::process::exit(exit_code);
    }

    let server_running = is_server_running(&socket_path);

    if server_running {
        if args.restart {
            send_restart(&socket_path)?;
            println!("Sent restart signal to running instance");
            thread::sleep(std::time::Duration::from_millis(500));
            if is_server_running(&socket_path) {
                let exit_code = attach_to_server(&socket_path)?;
                std::process::exit(exit_code);
            }
        } else {
            println!("Attaching to running instance...");
            let exit_code = attach_to_server(&socket_path)?;
            std::process::exit(exit_code);
        }
    }

    // Start as server
    let cwd = working_dir.to_string_lossy().to_string();
    let mut cache = TestCache::load();
    cache.touch(&cwd);

    let exit_code = run_server(socket_path, args.cmd, args.file)?;
    std::process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi_codes() {
        // Simple case
        assert_eq!(strip_ansi_codes("hello"), "hello");
        
        // With color codes
        assert_eq!(strip_ansi_codes("\x1b[32m✓\x1b[0m test"), "✓ test");
        
        // Complex vitest output
        let input = " \x1b[32m✓\x1b[0m \x1b[45mjsdom\x1b[0m src/test.ts (8 tests) 2ms";
        let clean = strip_ansi_codes(input);
        assert!(clean.trim().starts_with("✓"));
    }

    #[test]
    fn test_parse_completed_file_tests() {
        // Plain text
        assert_eq!(parse_completed_file_tests(" ✓ jsdom src/test.ts (8 tests) 2ms"), Some(8));
        
        // With ANSI codes
        let colored = " \x1b[32m✓\x1b[0m \x1b[45mjsdom\x1b[0m src/test.ts (15 tests) 2ms";
        assert_eq!(parse_completed_file_tests(colored), Some(15));
        
        // Non-matching line
        assert_eq!(parse_completed_file_tests(" ❯ jsdom src/test.ts"), None);
    }
}
