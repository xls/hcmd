"""Production line counts, excluding #[cfg(test)] tails and companion tests."""
import sys, pathlib
def prod(p):
    lines = p.read_text(errors="replace").split("\n")
    for i, l in enumerate(lines):
        if "#[cfg(test)]" in l:
            return i
    return len(lines)
files = [p for p in pathlib.Path("src").rglob("*.rs") if not p.name.endswith("tests.rs")]
if sys.argv[1] == "over300":
    print(sum(1 for p in files if prod(p) > 300))
elif sys.argv[1] == "app":
    print(sum(prod(p) for p in files if str(p).startswith("src/app")))
elif sys.argv[1] == "over100k":
    print(sum(1 for p in files if p.stat().st_size > 100_000))
elif sys.argv[1] == "list100k":
    for p in sorted(files, key=lambda q: -q.stat().st_size):
        if p.stat().st_size > 100_000:
            print(f"{p.stat().st_size // 1024:>5} KB  {prod(p):>5} lines  {p}")
