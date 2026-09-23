#!/bin/sh
# Checks the Windows build from Linux: clippy for x86_64-pc-windows-gnu, which
# needs only `rustup target add x86_64-pc-windows-gnu`. Nothing is linked, so the
# Windows resource compiler the app's build script calls is replaced by a stub
# that writes an empty resource file. The post-merge CI job on our Windows
# runner builds and links for real.
set -eu
stub_dir=$(mktemp -d)
trap 'rm -rf "$stub_dir"' EXIT
cat > "$stub_dir/windres" <<'EOF'
#!/bin/sh
# A stand-in for GNU windres: answers the version probe, writes an empty output.
for arg; do
  case "$arg" in -V) echo "GNU windres (stub)"; exit 0 ;; esac
done
while [ $# -gt 0 ]; do
  if [ "$1" = "--output" ]; then : > "$2"; exit 0; fi
  shift
done
EOF
chmod +x "$stub_dir/windres"
RC_x86_64_pc_windows_gnu="$stub_dir/windres" \
  cargo clippy --target x86_64-pc-windows-gnu --all-targets --locked "$@" -- -D warnings
