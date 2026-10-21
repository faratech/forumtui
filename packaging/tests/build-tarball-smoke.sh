#!/usr/bin/env bash
# Exercise the real packager with controlled compiler outputs, not a Rust build.
set -euo pipefail

packager="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../linux" && pwd)/build-tarball.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

run_case() {
    local name="$1" mode="$2" output_dir="$3"
    local fixture="$work/$name"
    mkdir -p "$fixture/packaging/linux" "$fixture/wftui" "$fixture/fake-bin" "$fixture/target/release"
    cp "$packager" "$fixture/packaging/linux/build-tarball.sh"
    printf 'version = "9.8.7"\n' > "$fixture/wftui/Cargo.toml"
    printf 'stale binary\n' > "$fixture/target/release/wftui"
    cat > "$fixture/fake-bin/rustc" <<'MOCK'
#!/bin/sh
printf 'host: x86_64-unknown-linux-gnu\n'
MOCK
    cat > "$fixture/fake-bin/cargo" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
target="${CARGO_BUILD_TARGET:-}"
directory="${CARGO_TARGET_DIR:-$PWD/ambient-config-target}"
features="builtin"
while (( $# )); do
    case "$1" in
        --target) target="$2"; shift 2 ;;
        --target-dir) directory="$2"; shift 2 ;;
        --features)
            case "$2" in
                images) features=generic ;;
                builtin-windowsforum) features=no-images ;;
            esac
            shift 2
            ;;
        *) shift ;;
    esac
done
output="$directory${target:+/$target}/release/wftui"
mkdir -p "$(dirname "$output")"
printf 'fresh %s %s\n' "$target" "$features" > "$output"
chmod 755 "$output"
MOCK
    chmod 755 "$fixture/fake-bin/"*
    local prefix=wftui suffix= variant=builtin
    local -a flags=()
    case "$mode" in
        generic) prefix=wftui-xf; variant=generic; flags=(--generic) ;;
        no-images) suffix=-no-images; variant=no-images; flags=(--no-images) ;;
    esac
    (
        cd "$fixture"
        export PATH="$fixture/fake-bin:$PATH"
        # The packager promises a native build even with an ambient target.
        export CARGO_BUILD_TARGET=aarch64-unknown-linux-gnu
        if [[ -n "$output_dir" ]]; then
            export CARGO_TARGET_DIR="$output_dir"
        else
            unset CARGO_TARGET_DIR
        fi
        bash packaging/linux/build-tarball.sh "${flags[@]}"
    )
    local package="$prefix-9.8.7-linux-x86_64$suffix"
    local expected="fresh x86_64-unknown-linux-gnu $variant"
    local actual
    actual="$(tar xOzf "$fixture/dist/$package.tar.gz" "$package/wftui")"
    [[ "$actual" == "$expected" ]] || { printf '%s: packaged stale or wrong-target binary\n' "$name" >&2; exit 1; }
    (cd "$fixture/dist" && sha256sum -c "$package.tar.gz.sha256")
    if [[ "$mode" != no-images ]]; then
        [[ "$(cat "$fixture/dist/$package")" == "$expected" ]]
        (cd "$fixture/dist" && sha256sum -c "$package.sha256")
    else
        [[ ! -e "$fixture/dist/wftui-9.8.7-linux-x86_64" ]]
    fi
    printf 'PASS %s\n' "$name"
}

run_case DefaultDirectory builtin ''
run_case CustomAbsoluteDirectory builtin "$work/custom output"
run_case GenericRelativeDirectory generic relative-build
run_case NoImagesNativeTarget no-images "$work/no-images output"
