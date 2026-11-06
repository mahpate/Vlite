# Opt-in faster local builds for this repository; nothing here is committed to Cargo configuration.
# Usage (this shell only; open a new shell, or run vlite_dev_fast_off, to undo; the file must be sourced):
#   . scripts/dev-fast-env.sh                              enable what is available and print it
#   VLITE_DEV_FAST_DEBUG=2 . scripts/dev-fast-env.sh       debug info level (default line-tables-only; 2 for a debugger)
#   VLITE_DEV_FAST_INCREMENTAL=1 . scripts/dev-fast-env.sh keep incremental builds on even with sccache
#   vlite_dev_fast_off                                     unset what this script set
# It sets, and never overrides a variable you already set: RUSTC_WRAPPER (sccache, if installed),
# CARGO_INCREMENTAL (off with sccache, which cannot cache incremental crates), CARGO_PROFILE_DEV_DEBUG and
# CARGO_PROFILE_TEST_DEBUG. The linker is left at the toolchain default. Changing these rebuilds the target
# directory once. CI does not use this script.

_vf_sourced=1
if [ -n "${ZSH_VERSION:-}" ]; then
    case "${ZSH_EVAL_CONTEXT:-}" in *:file) ;; *) _vf_sourced=0 ;; esac
elif [ -n "${BASH_VERSION:-}" ]; then
    [ "${BASH_SOURCE:-$0}" != "$0" ] || _vf_sourced=0
else
    case "${0##*/}" in dev-fast-env.sh) _vf_sourced=0 ;; esac
fi
if [ "$_vf_sourced" = 0 ]; then
    echo "dev-fast: source this file (. scripts/dev-fast-env.sh); running it would change nothing" >&2
    unset _vf_sourced
    # return ends a sourced file without closing the shell; exit only runs when the file was executed.
    return 1 2>/dev/null || exit 1
fi
unset _vf_sourced

vlite_dev_fast_off() {
    _vf_undone=0
    for _vf_name in RUSTC_WRAPPER CARGO_INCREMENTAL CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG; do
        case " ${VLITE_DEV_FAST_SET:-} " in
            *" $_vf_name "*)
                unset "$_vf_name"
                echo "dev-fast: unset $_vf_name"
                _vf_undone=1
                ;;
        esac
    done
    [ "$_vf_undone" = 1 ] || echo "dev-fast: nothing to undo"
    unset VLITE_DEV_FAST_SET _vf_name _vf_undone
}

_vf_applied=0
_vf_enable() {
    eval "_vf_current=\${$1:-}"
    if [ -n "$_vf_current" ]; then
        echo "dev-fast: kept your $1=$_vf_current"
        return 0
    fi
    export "$1=$2"
    VLITE_DEV_FAST_SET="${VLITE_DEV_FAST_SET:+$VLITE_DEV_FAST_SET }$1"
    export VLITE_DEV_FAST_SET
    _vf_applied=$((_vf_applied + 1))
    echo "dev-fast: $1=$2  ($3)"
}

if command -v sccache >/dev/null 2>&1; then
    _vf_enable RUSTC_WRAPPER sccache "compile cache, $(command -v sccache)"
else
    echo "dev-fast: sccache not found: no compile cache (install with: cargo install sccache --locked)"
fi

_vf_forced="${VLITE_DEV_FAST_INCREMENTAL:-}"
_vf_incremental="$_vf_forced"
case "${RUSTC_WRAPPER:-}" in
    *sccache*) [ -n "$_vf_incremental" ] || _vf_incremental=0 ;;
esac
if [ -n "$_vf_incremental" ]; then
    if [ -n "$_vf_forced" ]; then
        _vf_why="from VLITE_DEV_FAST_INCREMENTAL"
    else
        _vf_why="sccache cannot cache incremental crates; VLITE_DEV_FAST_INCREMENTAL=1 keeps them"
    fi
    _vf_enable CARGO_INCREMENTAL "$_vf_incremental" "$_vf_why"
else
    echo "dev-fast: CARGO_INCREMENTAL left at the Cargo default (incremental dev builds): fastest for edit and rebuild loops"
fi

_vf_debug="${VLITE_DEV_FAST_DEBUG:-line-tables-only}"
_vf_enable CARGO_PROFILE_DEV_DEBUG "$_vf_debug" "less debug info to write and link; panics still show file and line"
_vf_enable CARGO_PROFILE_TEST_DEBUG "$_vf_debug" "less debug info to write and link; panics still show file and line"

echo "dev-fast: linker unchanged (the override is for Windows)"
echo "dev-fast: $_vf_applied setting(s) applied; undo with: vlite_dev_fast_off"

unset -f _vf_enable
unset _vf_applied _vf_current _vf_forced _vf_incremental _vf_why _vf_debug
