# clove(1) bash completion.
# Install: clove completions bash > /etc/bash_completion.d/clove
_clove() {
    local cur cmds
    cur="${COMP_WORDS[COMP_CWORD]}"
    cmds="status list watch show add remove pause resume verify priorities announce sequential peer completions"
    if [ "${COMP_CWORD}" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "${cmds}" -- "${cur}") )
        return 0
    fi
    case "${COMP_WORDS[1]}" in
        add) COMPREPLY=( $(compgen -f -- "${cur}") ) ;;
        completions) COMPREPLY=( $(compgen -W "bash zsh fish" -- "${cur}") ) ;;
        sequential) [ "${COMP_CWORD}" -eq 3 ] && COMPREPLY=( $(compgen -W "on off" -- "${cur}") ) ;;
        # Torrents are named by info-hash or a unique prefix, which nothing
        # here can enumerate without talking to the daemon; the flags are what
        # completion can usefully offer.
        remove) COMPREPLY=( $(compgen -W "--all --data" -- "${cur}") ) ;;
        pause|resume|verify|announce) COMPREPLY=( $(compgen -W "--all" -- "${cur}") ) ;;
    esac
    return 0
}
complete -F _clove clove
