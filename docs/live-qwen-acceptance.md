# Live Ollama / Qwen acceptance demo

This is a reproducible **manual acceptance test**, not a benchmark or an automated security certification. It uses a disposable Rust library crate and a local Ollama-compatible model.

The goal is to show that a language model's proposed code edit is not treated as successful until the Rust host verifies a candidate, presents the exact diff, receives explicit approval, applies the edit, and checks the result. No cloud account or external API is required.

## Prerequisites

- Windows PowerShell, Rust stable/Cargo, and an Ollama service.
- A locally installed Ollama model capable of tool use. The example tag is `qwen-code-local:9b`; check `ollama list` and replace it if you use a different tag.
- Only run this exercise in a disposable, trusted crate. **Verification runs Cargo build scripts, procedural macros, and tests with your OS privileges. It is not a sandbox.**

## 1. Build the FİKO CODER executable

From the FİKO CODER checkout on the reviewed development branch:

```powershell
cargo build --release --locked
$FikoExe = (Resolve-Path '.\target\release\fiko_coder.exe').Path
```

## 2. Create a fresh disposable Rust project

In **the same PowerShell session**:

```powershell
$TestDir = Join-Path $env:TEMP ("fiko_handoff_" + [guid]::NewGuid().ToString('N').Substring(0,8))
cargo new "$TestDir" --lib --vcs none
if ($LASTEXITCODE -ne 0) { throw 'Could not create disposable project' }

$Utf8 = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText((Join-Path $TestDir 'src\lib.rs'), "pub mod math;`n", $Utf8)

$Math = @'
pub fn double(n: i32) -> i32 { n * 2 }
pub fn triple(n: i32) -> i32 { n * 3 }
pub fn quadruple(n: i32) -> i32 { n * 4 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn test_double() { assert_eq!(double(3), 6); }
    #[test] fn test_triple() { assert_eq!(triple(3), 9); }
    #[test] fn test_quadruple() { assert_eq!(quadruple(3), 12); }
}
'@
[System.IO.File]::WriteAllText((Join-Path $TestDir 'src\math.rs'), $Math + "`n", $Utf8)

Set-Location $TestDir
$LibHashBefore = (Get-FileHash '.\src\lib.rs' -Algorithm SHA256).Hash
cargo test
```

Expected baseline: **3 passed**, 0 failed.

## 3. Run the actual model

From the disposable project's directory:

```powershell
& $FikoExe qwen-code-local:9b
```

Enter this prompt at the agent's interactive `Sen >` prompt:

```text
Modify src/math.rs, preserve src/lib.rs without modifying it. Add pub fn quintuple(n: i32) -> i32 { n * 5 } and a unit test named test_quintuple to src/math.rs. Preserve all existing functions and tests. Show the verified diff, request explicit apply approval, then run cargo check and cargo test.
```

If prompted, permit verification **only because this is a fresh, trusted, disposable crate**. Review the entire displayed diff and explicitly approve application **only if `src/math.rs` is the sole changed file and the added code is correct**.

Expected host-side signals (exact formatting may evolve):

- `[NIYET] Modify` and `Korunan dosyalar (yazilamaz): src/lib.rs`
- `VERIFY_OK` / `VERIFY_WORKSPACE_OK` on a temporary workspace, with no original-project edit
- A diff scoped to `src/math.rs`
- Explicit human apply approval
- `PATCH_VERIFIED` after actual `cargo check` and `cargo test`

Exit with **`cikis`**, not `q`.

## 4. Independently inspect the result

Back in the same PowerShell session:

```powershell
cargo check
cargo test
$LibHashAfter = (Get-FileHash '.\src\lib.rs' -Algorithm SHA256).Hash
"lib.rs unchanged: $($LibHashBefore -eq $LibHashAfter)"
Select-String '.\src\math.rs' -Pattern 'quintuple|test_quintuple'
```

Pass criteria: 4 tests pass; `lib.rs unchanged: True`; `math.rs` contains both `quintuple` and `test_quintuple`. If the agent does not generate the required tool calls, refuses approval, or its verification fails, **do not count the run as a pass**.

## Evidence and limitations

A maintainer-run acceptance exercise on Windows on **2026-11-22** with `qwen-code-local:9b` met these pass criteria: 3 to 4 passing math tests, actual `PATCH_VERIFIED`, and the SHA-256 hash of `src/lib.rs` remained unchanged. This is a **single local acceptance run**; model performance and other machines are not guaranteed.

This demonstration exercised the **multi-file workspace path** (`verify_workspace` / `apply_workspace`) with one edited target. The distinct single-file handoff path (`verify_candidate` / `apply_patch`) has automated regression coverage but has **not** been separately live-validated with Qwen. PostgreSQL acceptance tests require an explicitly provisioned local test database and are excluded from default test runs.

For further limitations see [SECURITY.md](../SECURITY.md).
