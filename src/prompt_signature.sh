# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Preserve the private command directory even if .bashrc replaces PATH.
__beyrix_command_dir=${BEYRIX_COMMAND_DIR-}
[[ -r "$HOME/.bashrc" ]] && source "$HOME/.bashrc"
if [[ -n $__beyrix_command_dir ]]; then PATH="$__beyrix_command_dir${PATH:+:$PATH}"; export PATH; fi
unset BEYRIX_COMMAND_DIR __beyrix_command_dir
__beyrix_prompt() {
    local status=$? git_info branch dirty
    branch=$(command git symbolic-ref --quiet --short HEAD 2>/dev/null) || branch=$(command git rev-parse --short HEAD 2>/dev/null) || branch=''
    git_info=''
    [[ -z $branch ]] || git_info=$(command git status --porcelain=v1 2>/dev/null)
    dirty=''; [[ -n $git_info ]] && dirty=' *'
    __beyrix_git=''; [[ -z $branch ]] || __beyrix_git=" [$branch$dirty]"
    __beyrix_status=$status
    local __beyrix_style='minimal'
    [[ -r ${BEYRIX_PROMPT_THEME-} ]] && IFS= read -r __beyrix_style < "$BEYRIX_PROMPT_THEME"
    case $__beyrix_style in
      developer) PS1='\[\e[1;32m\]\u@\h\[\e[0m\] \[\e[1;34m\]${PWD}\[\e[0;33m\]${__beyrix_git:-${__beyrix_branch}}\[\e[0;31m\] [${__beyrix_status:-0}]\[\e[0m\]\n\$ ' ;;
      signature) PS1='\[\e[1;35m\]◆ BEYRIX\[\e[0;36m\] [${__beyrix_status:-0}]\[\e[0m\]\n\[\e[0;36m\]${PWD}\[\e[0;35m\]${__beyrix_git:-${__beyrix_branch}}\[\e[0m\]\n❯ ' ;;
      *) PS1='\[\e[0;36m\]${PWD}\[\e[0;90m\]${__beyrix_git:-${__beyrix_branch}}\[\e[0m\]\n\$ ' ;;
    esac
    return "$status"
}
PROMPT_COMMAND=(__beyrix_prompt "${PROMPT_COMMAND[@]}")
