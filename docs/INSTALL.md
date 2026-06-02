# Installing Nova

## Prerequisites

| Tool | Version | Purpose |
|------|---------|---------|
| Rust + Cargo | 1.75+ (stable) | Build the Nova compiler |
| clang / LLVM | 15+ | LLVM backend (native code generation) |
| Git | any | Clone the repository |

### Rust

Install via [rustup](https://rustup.rs):

```powershell
# Windows / macOS / Linux
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

On Windows you can also use the official installer from [rust-lang.org](https://www.rust-lang.org/tools/install).

Verify:

```
rustc --version   # rustc 1.75.0 or newer
cargo --version
```

### clang / LLVM

The LLVM backend (`nova run` default, `nova build`) requires clang in your PATH.

**Windows:**
```powershell
winget install LLVM.LLVM
```
Or download from [releases.llvm.org](https://releases.llvm.org).

**macOS:**
```bash
xcode-select --install       # ships clang with Xcode tools
# or
brew install llvm
```

**Linux (Debian/Ubuntu):**
```bash
sudo apt install clang
```

**Linux (Fedora/RHEL):**
```bash
sudo dnf install clang
```

**Linux (Arch):**
```bash
sudo pacman -S clang
```

Verify:
```
clang --version   # clang version 15.0.0 or newer
```

If clang is not installed, `nova run` falls back to the bytecode VM automatically and prints:

```
note: clang not found — running via VM. Install it for native speed:
  winget install LLVM.LLVM
```

Your program still runs. You only lose native-code performance until clang is installed.

---

## Build from Source

```bash
git clone https://github.com/jivixo/nova.git
cd nova
cargo build --release
```

The release binary is written to `target/release/nova` (Linux/macOS) or `target\release\nova.exe` (Windows).

The first release build takes 30–60 seconds; incremental rebuilds are a few seconds.

---

## Copy Binary to PATH

### Windows

```powershell
# Create a personal bin directory if it doesn't exist
New-Item -ItemType Directory -Force "$env:USERPROFILE\bin"

# Copy the binary
Copy-Item "target\release\nova.exe" "$env:USERPROFILE\bin\nova.exe"

# Add to PATH (one-time, for the current user)
$current = [System.Environment]::GetEnvironmentVariable("PATH", "User")
if ($current -notlike "*$env:USERPROFILE\bin*") {
    [System.Environment]::SetEnvironmentVariable(
        "PATH", "$current;$env:USERPROFILE\bin", "User")
}
```

Open a new terminal to pick up the updated PATH.

### macOS / Linux

```bash
sudo cp target/release/nova /usr/local/bin/nova
# or without sudo:
cp target/release/nova ~/.local/bin/nova   # ensure ~/.local/bin is in your PATH
```

---

## Verify Installation

Run the REPL:
```
nova
```

Expected:
```
Nova REPL — type 'quit' to exit
nova>
```

Run a file:
```
nova run hello.nova
```

Where `hello.nova` contains:
```nova
print "hello, world"
```

Expected output:
```
hello, world
```

Build a native binary:
```
nova build hello.nova
.\hello.exe        # Windows
./hello            # macOS / Linux
```

---

## Runtime Object (`nova_rt.o`)

The LLVM backend links against `nova_rt.o`, compiled from `nova_rt.c`. Nova builds this automatically the first time `nova run` or `nova build` is used, and rebuilds it whenever `nova_rt.c` changes. You never need to run `clang -c nova_rt.c` manually.

If you move the Nova binary to a machine without `nova_rt.c` nearby, copy both `nova` (the binary) and `nova_rt.c` to the same directory. Nova will find `nova_rt.c` relative to the binary and recompile `nova_rt.o` on demand.

---

## Execution Modes

| Command | Backend | Notes |
|---------|---------|-------|
| `nova run file.nova` | LLVM (default) | Compiles to temp binary, runs, deletes |
| `nova run --vm file.nova` | Bytecode VM | No clang required |
| `nova run --tree file.nova` | Tree-walking interpreter | Slowest; no recursion cap |
| `nova run --tree --memory file.nova` | Tree-walking interpreter | Prints allocation report to stderr after exit |
| `nova build file.nova` | LLVM | Keeps `file.exe` next to source |
| `nova` | REPL (bytecode VM) | Interactive; `quit` to exit |

`--jobs N` sets the rayon thread pool size for `spawn`/`wait` (default: one per CPU core). It applies to the VM and tree-walker backends only. The LLVM backend spawns OS threads directly via the C runtime and ignores it.

```
nova run --vm --jobs 4 file.nova
nova run --tree --jobs 4 file.nova
```
