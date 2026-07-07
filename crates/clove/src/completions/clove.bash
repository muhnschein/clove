# clove(1) bash completion.
# Install: clove completions bash > /etc/bash_completion.d/clove
_clove() {
    local cur cmds
    cur="${COMP_WORDS[COMP_CWORD]}"
    cmds="status list show add remove pause resume verify priorities completions"
    if [ "${COMP_CWORD}" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "${cmds}" -- "${cur}") )
        return 0
    fi
    case "${COMP_WORDS[1]}" in
        add) COMPREPLY=( $(compgen -f -- "${cur}") ) ;;
        completions) COMPREPLY=( $(compgen -W "bash zsh fish" -- "${cur}") ) ;;
    esac
    return 0
}
complete -F _clove clove
