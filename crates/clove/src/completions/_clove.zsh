#compdef clove
# clove(1) zsh completion.
# Install: clove completions zsh > "${fpath[1]}/_clove"
_clove() {
    local -a cmds
    cmds=(status list watch show add remove pause resume verify priorities announce sequential peer completions)
    _arguments '1: :->command' '*:: :->args'
    case $state in
        command) _describe 'clove command' cmds ;;
        args)
            case $words[1] in
                add) _files ;;
                completions) _values 'shell' bash zsh fish ;;
                sequential) [[ $CURRENT -eq 3 ]] && _values 'setting' on off ;;
            esac
            ;;
    esac
}
_clove "$@"
