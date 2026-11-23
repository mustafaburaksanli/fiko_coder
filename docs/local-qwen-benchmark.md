# Repeatable local Qwen acceptance measurement

This provides a human-supervised, two-phase check for FİKO CODER's existing math-project example. The script never calls Ollama, never types model responses, never approves a patch and never deletes projects. It records machine-readable independent results after a real model run.

**Task:** Update src/math.rs by adding a public quintuple function (multiply by 5) and a unit test named test_quintuple, while preserving existing math methods/tests and the SHA-256 of src/lib.rs.

**Requirements:** Windows PowerShell (PowerShell 7 recommended), installed Rust stable/Cargo, local Ollama, an installed tool-capable model. Use only disposable projects. Cargo build scripts and tests execute with your OS privileges; this is not a sandbox.

## One trial

Build FİKO CODER from its repository:

~~~powershell
cargo build --release --locked
$FikoExe = (Resolve-Path './target/release/fiko_coder.exe').Path
$Acceptance = (Resolve-Path './scripts/live_qwen_acceptance.ps1').Path
$Fixture = Join-Path $env:TEMP ('fiko_live_' + [guid]::NewGuid().ToString('N').Substring(0,8))
& $Acceptance -Mode prepare -FixtureDir $Fixture
~~~

Preparation creates a fresh library crate, writes 3 working math tests, runs actual Cargo tests offline, and records protected-source and manifest hashes in a separate baseline JSON next to the fixture. Existing fixture and baseline paths are never overwritten.

Run the agent yourself in the disposable project:

~~~powershell
Push-Location $Fixture
& $FikoExe qwen-code-local:9b
Pop-Location
~~~

At the agent prompt enter:

> Modify src/math.rs, preserve src/lib.rs without modifying it. Add pub fn quintuple(n: i32) -> i32 { n * 5 } and a unit test named test_quintuple to src/math.rs. Preserve the existing three functions and tests. Verify the exact diff and ask for approval before applying, then run cargo check and cargo test.

Review the actual diff. Grant verification/execution and application approvals only if the candidate is safe and the written targets match the request. Type **cikis** to exit.

Evaluate independently:

~~~powershell
& $Acceptance -Mode evaluate -FixtureDir $Fixture
~~~

Checks include SHA-256 of protected source and manifest, exactly the expected Rust source paths, the new function and test names, preservation of the three previous function/test names, absence of an unresolved pending recovery marker, successful real Cargo test execution, and 4 tests passed with none failed. A structured JSON result is written beside the crate; exit code is 0 only if all checks pass.

## Multiple trials

Use a new randomized fixture for each attempt. Retain every result JSON. Separately record model tag and digest (if known), FİKO commit, operating system, the prompt, user approvals and whether tool calls were actually made. Do not store credentials or private code in shared reports.

For N independent attempts, strict task success rate is **number of pass=true results / N**, including refusals, tool-call failures, approval rejections, timeouts and incomplete results as unsuccessful. Never discard failed trials or silently retry them.

**Limits:** A source-name check and four passing tests cannot prove semantic correctness, protected contents outside the two hashed files, model independence, or production security. Inspect terminal transcripts and diffs separately. Run dedicated benchmarks for new-file creation, compiler/test repair, multi-file changes, and eventual website development. This example alone cannot establish website-building capability.

For the original human walkthrough and the historical single-run acceptance claim, see [live-qwen-acceptance.md](live-qwen-acceptance.md). The new script does not retroactively validate that prior run.
