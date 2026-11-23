# Preserve the private command directory even if .bashrc replaces PATH.
__beynal_command_dir=${BEYNAL_COMMAND_DIR-}
[[ -r "$HOME/.bashrc" ]] && source "$HOME/.bashrc"
if [[ -n $__beynal_command_dir ]]; then PATH="$__beynal_command_dir${PATH:+:$PATH}"; export PATH; fi
unset BEYNAL_COMMAND_DIR __beynal_command_dir
__beynal_prompt() {
    local status=$? git_info branch dirty
    branch=$(command git symbolic-ref --quiet --short HEAD 2>/dev/null) || branch=$(command git rev-parse --short HEAD 2>/dev/null) || branch=''
    git_info=''
    [[ -z $branch ]] || git_info=$(command git status --porcelain=v1 2>/dev/null)
    dirty=''; [[ -n $git_info ]] && dirty=' *'
    __beynal_git=''; [[ -z $branch ]] || __beynal_git=" [$branch$dirty]"
    __beynal_status=$status
    local __beynal_style='minimal'
    [[ -r ${BEYNAL_PROMPT_THEME-} ]] && IFS= read -r __beynal_style < "$BEYNAL_PROMPT_THEME"
    case $__beynal_style in
      developer) PS1='\[\e[1;32m\]\u@\h\[\e[0m\] \[\e[1;34m\]${PWD}\[\e[0;33m\]${__beynal_git:-${__beynal_branch}}\[\e[0;31m\] [${__beynal_status:-0}]\[\e[0m\]\n\$ ' ;;
      signature) PS1='\[\e[1;35m\]◆ BEYNAL\[\e[0;36m\] [${__beynal_status:-0}]\[\e[0m\]\n\[\e[0;36m\]${PWD}\[\e[0;35m\]${__beynal_git:-${__beynal_branch}}\[\e[0m\]\n❯ ' ;;
      *) PS1='\[\e[0;36m\]${PWD}\[\e[0;90m\]${__beynal_git:-${__beynal_branch}}\[\e[0m\]\n\$ ' ;;
    esac
    return "$status"
}
PROMPT_COMMAND=(__beynal_prompt "${PROMPT_COMMAND[@]}")
