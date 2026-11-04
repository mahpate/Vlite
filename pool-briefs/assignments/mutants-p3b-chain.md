# Verification job: P3b seam chain mutation campaign (changes no source)

Your checkout is the candidate source (the P3b seam chain) with a verification kit under `pool-briefs/verify/`.
You change NO source file and NO script. The only file you write in the checkout is your assigned report file.

## Steps

1. `git status --porcelain` must be empty. Note `git rev-parse HEAD`.
2. Run this from the checkout root (PowerShell). Replace IDS and NAME with the values in your task prompt:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File pool-briefs/verify/pool-mutate.ps1 `
     -Mutants pool-briefs/verify/mut-p3b-chain-list.json -Ids "IDS" -Name NAME
   ```

   The script copies the checkout to a temporary folder outside it, builds and tests there with cargo, applies one
   mutant at a time, restores it and prints a markdown summary. A cold build comes first, so expect 30 to 90
   minutes. Start it as a background process whose output goes to a file under `$env:TEMP`, and poll that file; never
   run two cargo commands at once. It is finished when the output contains a line starting `SUMMARY-FILE`.
3. Copy the printed markdown summary, unchanged, into your assigned report file, and add one last line with the
   output of `rustc --version`.
4. If the script cannot start, or its baseline fails, write the exact error into the report and stop. Do not try to
   repair the script or the source.

## Verdicts

`KILLED` means a test failed (good). `SURVIVED` means no test failed: a real test gap, report it, do not fix it.
`COMPILE-ERROR`, `SKIP` and `TIMEOUT` must be reported as they are.
