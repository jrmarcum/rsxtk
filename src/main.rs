use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::header::USER_AGENT;
use serde_json::Value;
use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::{self, WasiP1Ctx};

#[derive(Parser)]
#[command(
    name = "rsxtk", 
    about = "🦀 Rust WASM Toolkit: High-performance manager for Rust WASI scripts.",
    version = "0.3.3"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
#[command(rename_all = "lowercase")]
enum Commands {
    /// 🏃 run a script, WAT, or WASM binary with auto-pipeline.
    Run { path: PathBuf },
    /// 🔨 build a specific output format (wasi, wasm, cwasm).
    Build { path: PathBuf, target: BuildTarget },
    /// 🔄 convert between .wasm and .wat formats (autonames unless -o is used).
    Convert { 
        input: PathBuf, 
        #[arg(short, long)]
        output: Option<PathBuf> 
    },
    /// ✨ initialize a new script file (.rs) with frontmatter.
    Init { name: String },
    /// 📦 initialize a new library module template.
    #[command(name = "init-mod")]
    InitMod { name: String },
    /// ⏱️ benchmark execution speed.
    Bench { path: PathBuf, #[arg(short, long, default_value = "5")] iterations: u32 },
    /// ➕ add a dependency to the script frontmatter.
    Add { 
        path: PathBuf, 
        crate_name: String, 
        /// Optional version (defaults to latest from crates.io)
        version: Option<String> 
    },
    /// ➖ remove a dependency from the script frontmatter.
    Remove { path: PathBuf, crate_name: String },
    /// 📦 list script dependencies.
    List { path: PathBuf },
    /// ✨ format the Rust code within a script.
    Fmt { path: PathBuf },
    /// 🧹 optimize a WASM file for size via Walrus.
    Optimize { #[arg(short, long)] input: PathBuf, #[arg(short, long)] output: Option<PathBuf> },
    /// 🗑️ wipe the .tk build cache.
    Clean,
    /// 📝 view WASM module metadata.
    Info { path: PathBuf },
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
#[value(rename_all = "lowercase")]
enum BuildTarget { Wasi, Wasm, Cwasm }

struct MyState { wasi: WasiP1Ctx }

fn main() -> Result<()> {
    let cli = Cli::parse();
    let engine = Engine::default();

    match cli.command {
        Commands::Run { path } => run_wasm(&path),
        Commands::Build { path, target } => match target {
            BuildTarget::Wasi | BuildTarget::Wasm => { build_with_virtual_cargo(&engine, &path).map(|_| ()) }
            BuildTarget::Cwasm => { compile_to_cwasm(&engine, &path, None).map(|_| ()) }
        },
        Commands::Convert { input, output } => convert_wasm_wat(&input, output),
        Commands::Init { name } => init_script(&name, false),
        Commands::InitMod { name } => init_script(&name, true),
        Commands::Bench { path, iterations } => benchmark_wasm(&path, iterations),
        Commands::Add { path, crate_name, version } => {
            let ver = match version {
                Some(v) => v,
                None => {
                    println!("🔍 Fetching latest version for {}...", crate_name);
                    get_latest_version(&crate_name)
                }
            };
            edit_deps(&path, &crate_name, Some(&ver))
        },
        Commands::Remove { path, crate_name } => edit_deps(&path, &crate_name, None),
        Commands::List { path } => list_deps(&path),
        Commands::Fmt { path } => format_script(&path),
        Commands::Optimize { input, output } => optimize_wasm(&input, output),
        Commands::Clean => clean_cache(),
        Commands::Info { path } => print_info(&engine, &path),
    }
}

// --- NEW HELPER ---

fn get_latest_version(crate_name: &str) -> String {
    let url = format!("https://crates.io/api/v1/crates/{}", crate_name);
    let client = reqwest::blocking::Client::new();

    let response = client
        .get(url)
        .header(USER_AGENT, "rsxtk (github.com/jrmarcum/rsxtk)")
        .send();

    match response {
        Ok(res) => {
            if let Ok(json) = res.json::<Value>() {
                if let Some(v) = json["crate"]["max_version"].as_str() {
                    return v.to_string();
                }
            }
            "*".to_string()
        }
        Err(_) => "*".to_string(), 
    }
}

// --- CORE DISPATCHER ---

fn run_wasm(path: &Path) -> Result<()> {
    let engine = Engine::default();
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");

    let artifact = match ext {
        "rs" => build_and_cache_script(&engine, path)?,
        "wat" => {
            println!("📑 WAT detected. Compiling to binary...");
            let wat_data = fs::read(path)?;
            let wasm_data = wat::parse_bytes(&wat_data)?;
            let wasm_path = path.with_extension("wasm");
            fs::write(&wasm_path, wasm_data.as_ref())?;
            compile_to_cwasm(&engine, &wasm_path, None)?
        },
        "wasm" => compile_to_cwasm(&engine, path, None)?,
        "cwasm" => path.to_path_buf(),
        _ => bail!("Unsupported file type: .{}", ext),
    };

    println!("🏃 Executing {}...", artifact.display());
    let module = unsafe { Module::deserialize_file(&engine, &artifact)? };
    let mut linker: Linker<MyState> = Linker::new(&engine);
    p1::add_to_linker_sync(&mut linker, |state| &mut state.wasi)?;

    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdio().inherit_args();
    let cur_dir = std::env::current_dir()?;
    builder.preopened_dir(&cur_dir, ".", wasmtime_wasi::DirPerms::all(), wasmtime_wasi::FilePerms::all())?;
    
    let mut store = Store::new(&engine, MyState { wasi: builder.build_p1() });
    let instance = linker.instantiate(&mut store, &module)?;
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    start.call(&mut store, ())?;
    Ok(())
}

// --- UTILITIES ---

fn convert_wasm_wat(input: &Path, output: Option<PathBuf>) -> Result<()> {
    let input_data = fs::read(input)?;
    let input_ext = input.extension().and_then(|s| s.to_str()).unwrap_or("");
    let (target_ext, is_to_wat) = match input_ext {
        "wasm" => ("wat", true),
        "wat" => ("wasm", false),
        _ => bail!("Input must be .wasm or .wat"),
    };
    let final_output = output.unwrap_or_else(|| input.with_extension(target_ext));

    if is_to_wat {
        let wat = wasmprinter::print_bytes(&input_data)?;
        fs::write(&final_output, wat)?;
    } else {
        let wasm = wat::parse_bytes(&input_data)?;
        fs::write(&final_output, wasm.as_ref())?;
    }
    println!("✅ Converted to {}", final_output.display());
    Ok(())
}

fn list_deps(path: &Path) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let parts: Vec<&str> = content.split("---").collect();
    if parts.len() < 3 { bail!("No manifest found."); }
    println!("📦 Dependencies for {}:\n{}", path.display(), parts[1].trim());
    Ok(())
}

fn build_and_cache_script(engine: &Engine, script_path: &Path) -> Result<PathBuf> {
    let stem = script_path.file_stem().unwrap().to_str().unwrap();
    let current_dir = std::env::current_dir()?;
    let cache_root = current_dir.join(".tk").join(stem);
    let cwasm_path = cache_root.join(format!("{}.cwasm", stem));

    // 1. Check if cache is still valid
    if cwasm_path.exists() {
        let script_mtime = fs::metadata(script_path)?.modified()?;
        let cwasm_mtime = fs::metadata(&cwasm_path)?.modified()?;
        if cwasm_mtime > script_mtime { return Ok(cwasm_path); }
    }

    let abs_cache = fs::canonicalize(&current_dir).unwrap_or(current_dir).join(".tk").join(stem);
    let abs_src = abs_cache.join("src");
    fs::create_dir_all(&abs_src)?;

    let content = fs::read_to_string(script_path)?;
    let parts: Vec<&str> = content.split("---").collect();

    let (cargo_deps, rust_src) = if parts.len() >= 3 {
        // Manifest exists: parts[1] is the manifest, parts[2..] is the source
        (parts[1].to_string(), parts[2..].join("---"))
    } else {
        // No manifest found: Check for 'use' statements in the whole file
        let has_use_statements = content.lines().any(|line| line.trim().starts_with("use "));
        
        if has_use_statements {
            // Found 'use' but no manifest: Error out
            bail!("Error: No dependency manifest onboard.");
        } else {
            // No 'use' and no manifest: Proceed with blank dependencies
            (String::from("[dependencies]"), content)
        }
    };

    // 2. Generate the temporary Cargo project
    let cargo_toml = format!(
        "[package]\nname=\"script\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\n{}", 
        cargo_deps
    );
    
    fs::write(abs_cache.join("Cargo.toml"), cargo_toml)?;
    fs::write(abs_src.join("main.rs"), rust_src)?;

    // 3. Compile the script
    println!("🔨 Compiling Rust...");
    let status = std::process::Command::new("cargo")
        .env("CARGO_TARGET_DIR", &abs_cache.join("t"))
        .args([
            "build", 
            "--manifest-path", abs_cache.join("Cargo.toml").to_str().unwrap(), 
            "--target", "wasm32-wasip1", 
            "--release"
        ])
        .status()?;

    if !status.success() { bail!("Cargo build failed."); }
    let wasm_out = abs_cache.join("t/wasm32-wasip1/release/script.wasm");
    compile_to_cwasm(engine, &wasm_out, Some(cwasm_path))
}

fn compile_to_cwasm(engine: &Engine, input: &Path, output: Option<PathBuf>) -> Result<PathBuf> {
    let out = output.unwrap_or_else(|| input.with_extension("cwasm"));
    let bytes = fs::read(input)?;
    let cwasm_bytes = engine.precompile_module(&bytes)?;
    fs::write(&out, cwasm_bytes)?;
    Ok(out)
}

fn build_with_virtual_cargo(engine: &Engine, path: &Path) -> Result<PathBuf> {
    let _ = build_and_cache_script(engine, path)?;
    let stem = path.file_stem().unwrap().to_str().unwrap();
    let wasm_path = std::env::current_dir()?.join(".tk").join(stem).join("t/wasm32-wasip1/release/script.wasm");
    let local_wasm = path.with_extension("wasm");
    fs::copy(wasm_path, &local_wasm)?;
    println!("✅ Built: {}", local_wasm.display());
    Ok(local_wasm)
}

fn init_script(name: &str, is_mod: bool) -> Result<()> {
    let filename = if name.ends_with(".rs") { name.to_string() } else { format!("{}.rs", name) };
    let content = if is_mod {
        "---\n[dependencies]\n---\npub fn execute() { println!(\"Module loaded.\"); }"
    } else {
        "---\n[dependencies]\n---\nfn main() { println!(\"Hello, rsxtk!\"); }"
    };
    fs::write(&filename, content)?;
    println!("✅ Created {}", filename);
    Ok(())
}

fn format_script(path: &Path) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let parts: Vec<&str> = content.split("---").collect();
    if parts.len() < 3 { bail!("Invalid format."); }
    let temp = path.with_extension("rs.tmp");
    fs::write(&temp, parts[2..].join("---"))?;
    let _ = std::process::Command::new("rustfmt").arg(&temp).status();
    let fmt_code = fs::read_to_string(&temp)?;
    let _ = fs::remove_file(temp);
    fs::write(path, format!("---\n{}\n---\n{}", parts[1].trim(), fmt_code))?;
    println!("✨ Formatted.");
    Ok(())
}

fn edit_deps(path: &Path, crate_name: &str, version: Option<&str>) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let parts: Vec<&str> = content.split("---").collect();
    if parts.len() < 3 { bail!("Invalid format."); }
    let mut lines: Vec<String> = parts[1].lines().filter(|l| !l.trim().starts_with(crate_name)).map(|s| s.to_string()).collect();
    if let Some(v) = version { lines.push(format!("{} = \"{}\"", crate_name, v)); }
    fs::write(path, format!("---\n{}\n---\n{}", lines.join("\n").trim(), parts[2..].join("---")))?;
    println!("✅ Dependencies updated.");
    Ok(())
}

fn benchmark_wasm(path: &Path, iterations: u32) -> Result<()> {
    println!("⏱️ Benchmarking {}...", path.display());
    let start = Instant::now();
    for _ in 0..iterations { run_wasm(path)?; }
    println!("📊 Avg Execution: {:?}", start.elapsed() / iterations);
    Ok(())
}

fn clean_cache() -> Result<()> {
    let cache = std::env::current_dir()?.join(".tk");
    if cache.exists() { fs::remove_dir_all(cache)?; println!("✨ Cache cleaned."); }
    Ok(())
}

fn print_info(engine: &Engine, path: &Path) -> Result<()> {
    let bytes = fs::read(path)?;
    let module = Module::new(engine, bytes)?;
    println!("📝 Imports: {} | Exports: {}", module.imports().count(), module.exports().count());
    Ok(())
}

fn optimize_wasm(input: &Path, output: Option<PathBuf>) -> Result<()> {
    let out = output.unwrap_or_else(|| input.to_path_buf());
    let bytes = fs::read(input)?;
    let mut module = walrus::Module::from_buffer(&bytes)?;
    fs::write(out, module.emit_wasm())?;
    println!("✅ Optimized.");
    Ok(())
}