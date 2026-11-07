# Opt-in faster local builds for this repository; nothing here is committed to Cargo configuration.
# Usage (this shell only; open a new shell, or run vlite_dev_fast_off, to undo; the file must be sourced):
#   . scripts/dev-fast-env.sh                              enable what is available and print it
#   VLITE_DEV_FAST_DEBUG=2 . scripts/dev-fast-env.sh       debug info level (default line-tables-only; 2 for a debugger)
#   VLITE_DEV_FAST_INCREMENTAL=1 . scripts/dev-fast-env.sh 1 or 0: incremental builds on (release too) or off, even with sccache
#   vlite_dev_fast_off                                     unset what this script set
# It sets, and never overrides a variable you already set, even an empty one (a value it set itself is
# replaced when you source it again): RUSTC_WRAPPER (sccache, if installed), CARGO_INCREMENTAL (off with
# sccache, which cannot cache incremental crates), CARGO_PROFILE_DEV_DEBUG and CARGO_PROFILE_TEST_DEBUG. The
# linker is left at the toolchain default. Changing these rebuilds the target directory once. Not used by CI.

# Only a file that is positively being executed is refused: source and eval both carry on.
_vf_executed=0
if [ -n "${ZSH_VERSION:-}" ]; then
    case "${ZSH_EVAL_CONTEXT:-toplevel}" in toplevel) _vf_executed=1 ;; esac
elif [ -n "${BASH_VERSION:-}" ]; then
    if [ -n "${BASH_SOURCE:-}" ] && [ "${BASH_SOURCE:-}" = "$0" ]; then _vf_executed=1; fi
else
    case "${0##*/}" in dev-fast-env.sh) _vf_executed=1 ;; esac
fi
if [ "$_vf_executed" = 1 ]; then
    echo "dev-fast: source this file (. scripts/dev-fast-env.sh); running it would change nothing" >&2
    unset _vf_executed
    # An executed file ends with exit; return is tried first for a file that was sourced from a function.
    return 1 2>/dev/null || exit 1
fi
unset _vf_executed

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
    eval "_vf_isset=\${$1+set}"
    eval "_vf_current=\${$1:-}"
    case " ${VLITE_DEV_FAST_SET:-} " in
        *" $1 "*) _vf_ours=1 ;;
        *) _vf_ours=0 ;;
    esac
    if [ -n "$_vf_isset" ] && [ "$_vf_ours" = 0 ]; then
        echo "dev-fast: kept your $1=$_vf_current"
        return 0
    fi
    export "$1=$2"
    if [ "$_vf_ours" = 0 ]; then
        VLITE_DEV_FAST_SET="${VLITE_DEV_FAST_SET:+$VLITE_DEV_FAST_SET }$1"
        export VLITE_DEV_FAST_SET
    fi
    _vf_applied=$((_vf_applied + 1))
    echo "dev-fast: $1=$2  ($3)"
}

if command -v sccache >/dev/null 2>&1; then
    _vf_enable RUSTC_WRAPPER sccache "compile cache, $(command -v sccache)"
else
    echo "dev-fast: sccache not found: no compile cache (install with: cargo install sccache --locked)"
fi

_vf_forced="${VLITE_DEV_FAST_INCREMENTAL:-}"
case "$_vf_forced" in
    0 | 1 | "") ;;
    *)
        echo "dev-fast: VLITE_DEV_FAST_INCREMENTAL must be 0 or 1, not '$_vf_forced': ignored"
        _vf_forced=""
        ;;
esac
_vf_incremental="$_vf_forced"
case "${RUSTC_WRAPPER:-}" in
    *sccache*) [ -n "$_vf_incremental" ] || _vf_incremental=0 ;;
esac
if [ -n "$_vf_incremental" ]; then
    if [ "$_vf_forced" = 1 ]; then
        _vf_why="from VLITE_DEV_FAST_INCREMENTAL; release builds become incremental too"
    elif [ -n "$_vf_forced" ]; then
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
unset _vf_applied _vf_current _vf_forced _vf_incremental _vf_isset _vf_ours _vf_why _vf_debug
