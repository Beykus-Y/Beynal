# Preserve the private command directory even if .bashrc replaces PATH.
__beynal_command_dir=${BEYNAL_COMMAND_DIR-}
[[ -r "$HOME/.bashrc" ]] && source "$HOME/.bashrc"
if [[ -n $__beynal_command_dir ]]; then PATH="$__beynal_command_dir${PATH:+:$PATH}"; export PATH; fi
unset BEYNAL_COMMAND_DIR __beynal_command_dir

# READLINE_POINT and Bash substring offsets use characters in the active locale.
# Match only the element touching the cursor on the requested side.
__beynal_edit_element() {
    local point=$READLINE_POINT side element target
    local blank=$'[ \t]' word=$'[! \t]'

    if [[ $1 == left ]]; then
        side=${READLINE_LINE:0:point}
        if [[ $side == *$blank ]]; then
            element=${side##*$word}
        else
            element=${side##*$blank}
        fi
        target=$((point - ${#element}))
    else
        side=${READLINE_LINE:point}
        if [[ $side == $blank* ]]; then
            element=${side%%$word*}
        else
            element=${side%%$blank*}
        fi
        target=$((point + ${#element}))
    fi

    if [[ $2 == move ]]; then
        READLINE_POINT=$target
    elif (( target != point )); then
        local first=$point last=$target
        if (( target < point )); then
            first=$target last=$point
        fi
        READLINE_LINE=${READLINE_LINE:0:first}${READLINE_LINE:last}
        READLINE_POINT=$first
        if (( ${READLINE_MARK:-0} > last )); then
            READLINE_MARK=$((READLINE_MARK - (last - first)))
        elif (( ${READLINE_MARK:-0} > first )); then
            READLINE_MARK=$first
        fi
    fi
}

if [[ $- == *i* ]]; then
    for __beynal_keymap in emacs-standard vi-insert vi-command; do
        bind -m "$__beynal_keymap" -x '"\e[127;5u":__beynal_edit_element left delete'
        bind -m "$__beynal_keymap" -x '"\e[3;5~":__beynal_edit_element right delete'
        bind -m "$__beynal_keymap" -x '"\e[1;5D":__beynal_edit_element left move'
        bind -m "$__beynal_keymap" -x '"\e[1;5C":__beynal_edit_element right move'
    done
    unset __beynal_keymap
fi

__beynal_prompt() {
    local status=$? style=minimal compact=false branch dirty='' path
    if [[ -r ${BEYNAL_PROMPT_THEME-} ]]; then
        {
            IFS= read -r style
            IFS= read -r compact
        } < "$BEYNAL_PROMPT_THEME"
    fi

    path=$PWD
    if [[ -n ${HOME-} && $PWD == "$HOME" ]]; then
        path='~'
    elif [[ -n ${HOME-} && $PWD == "$HOME/"* ]]; then
        path="~/${PWD#"$HOME"/}"
    fi
    if [[ $style == minimal && $path != '~' && $path != '/' ]]; then
        path=${path##*/}
    fi
    __beynal_path=${path//[[:cntrl:]]/?}

    branch=$(command git symbolic-ref --quiet --short HEAD 2>/dev/null) ||
        branch=$(command git rev-parse --short HEAD 2>/dev/null) || branch=''
    if [[ $style != minimal && -n $branch ]]; then
        if [[ -n $(command git --no-optional-locks status --porcelain=v1 2>/dev/null | command head -n 1) ]]; then
            dirty=' *'
        fi
    fi

    __beynal_git=''
    __beynal_status=''
    case $style in
        developer)
            [[ -z $branch ]] || __beynal_git=" | $branch$dirty"
            (( status == 0 )) || __beynal_status=" | exit $status"
            PS1='\[\e[0;32m\]\u@\h\[\e[0m\] \[\e[0;34m\]${__beynal_path}\[\e[0;36m\]${__beynal_git}\[\e[0;31m\]${__beynal_status}\[\e[0m\] \$ '
            ;;
        signature)
            [[ -z $branch ]] || __beynal_git="  ⎇ $branch$dirty"
            (( status == 0 )) || __beynal_status="  exit $status"
            PS1='\[\e[0;35m\]◆\[\e[0;36m\] ${__beynal_path}\[\e[0;35m\]${__beynal_git}\[\e[0;91m\]${__beynal_status}'
            if [[ $compact == true ]]; then
                PS1+='\[\e[0;35m\] ❯\[\e[0m\] '
            else
                PS1+='\[\e[0;35m\]\n❯\[\e[0m\] '
            fi
            ;;
        *)
            [[ -z $branch ]] || __beynal_git=" · $branch"
            PS1='\[\e[0;36m\]${__beynal_path}\[\e[0;90m\]${__beynal_git}\[\e[0m\] \$ '
            ;;
    esac
    return "$status"
}
PROMPT_COMMAND=(__beynal_prompt "${PROMPT_COMMAND[@]}")
