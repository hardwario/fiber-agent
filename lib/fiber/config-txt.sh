# shellcheck shell=bash
# config_txt_apply FILE FRAGMENT
#   Ensure every non-comment, non-empty line of FRAGMENT is present in FILE as
#   a whole line (any section counts). Missing lines are appended under [all],
#   which is created at the end when absent. Prints "added: <line>" per line
#   it added; prints nothing when nothing changed. Idempotent.

config_txt_apply() {
    local file="$1" fragment="$2" line missing=()
    [ -f "$file" ] || fiber_die "config_txt_apply: $file does not exist"
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in ''|'#'*) continue ;; esac
        grep -qxF -- "$line" "$file" || missing+=("$line")
    done < "$fragment"
    [ "${#missing[@]}" -eq 0 ] && return 0
    # Append under [all]: the last section header must be [all], or a new
    # [all] block is opened (a repeated [all] is legal in config.txt).
    if [ "$(grep '^\[' "$file" | tail -n 1)" != "[all]" ]; then
        printf '[all]\n' >> "$file"
    fi
    for line in "${missing[@]}"; do
        printf '%s\n' "$line" >> "$file"
        printf 'added: %s\n' "$line"
    done
}
