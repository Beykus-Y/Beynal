# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Preserve the private command directory even if .bashrc replaces PATH.
__beyrix_command_dir=${BEYRIX_COMMAND_DIR-}
[[ -r "$HOME/.bashrc" ]] && source "$HOME/.bashrc"
if [[ -n $__beyrix_command_dir ]]; then PATH="$__beyrix_command_dir${PATH:+:$PATH}"; export PATH; fi
unset BEYRIX_COMMAND_DIR __beyrix_command_dir

# READLINE_POINT and Bash substring offsets use characters in the active locale.
# Match only the element touching the cursor on the requested side.
__beyrix_edit_element() {
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
    for __beyrix_keymap in emacs-standard vi-insert vi-command; do
        bind -m "$__beyrix_keymap" -x '"\e[127;5u":__beyrix_edit_element left delete'
        bind -m "$__beyrix_keymap" -x '"\e[3;5~":__beyrix_edit_element right delete'
        bind -m "$__beyrix_keymap" -x '"\e[1;5D":__beyrix_edit_element left move'
        bind -m "$__beyrix_keymap" -x '"\e[1;5C":__beyrix_edit_element right move'
    done
    unset __beyrix_keymap
fi

__beyrix_prompt() {
    local status=$? style=minimal compact=false branch dirty='' path
    if [[ -r ${BEYRIX_PROMPT_THEME-} ]]; then
        {
            IFS= read -r style
            IFS= read -r compact
        } < "$BEYRIX_PROMPT_THEME"
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
    __beyrix_path=${path//[[:cntrl:]]/?}

    branch=$(command git symbolic-ref --quiet --short HEAD 2>/dev/null) ||
        branch=$(command git rev-parse --short HEAD 2>/dev/null) || branch=''
    if [[ $style != minimal && -n $branch ]]; then
        if [[ -n $(command git --no-optional-locks status --porcelain=v1 2>/dev/null | command head -n 1) ]]; then
            dirty=' *'
        fi
    fi

    __beyrix_git=''
    __beyrix_status=''
    case $style in
        developer)
            [[ -z $branch ]] || __beyrix_git=" | $branch$dirty"
            (( status == 0 )) || __beyrix_status=" | exit $status"
            PS1='\[\e[0;32m\]\u@\h\[\e[0m\] \[\e[0;34m\]${__beyrix_path}\[\e[0;36m\]${__beyrix_git}\[\e[0;31m\]${__beyrix_status}\[\e[0m\] \$ '
            ;;
        signature)
            [[ -z $branch ]] || __beyrix_git="  ⎇ $branch$dirty"
            (( status == 0 )) || __beyrix_status="  exit $status"
            PS1='\[\e[0;35m\]◆\[\e[0;36m\] ${__beyrix_path}\[\e[0;35m\]${__beyrix_git}\[\e[0;91m\]${__beyrix_status}'
            if [[ $compact == true ]]; then
                PS1+='\[\e[0;35m\] ❯\[\e[0m\] '
            else
                PS1+='\[\e[0;35m\]\n❯\[\e[0m\] '
            fi
            ;;
        *)
            [[ -z $branch ]] || __beyrix_git=" · $branch"
            PS1='\[\e[0;36m\]${__beyrix_path}\[\e[0;90m\]${__beyrix_git}\[\e[0m\] \$ '
            ;;
    esac
    return "$status"
}
PROMPT_COMMAND=(__beyrix_prompt "${PROMPT_COMMAND[@]}")
