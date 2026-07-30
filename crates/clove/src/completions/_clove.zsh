#compdef clove
# clove(1) zsh completion.
# Install: clove completions zsh > "${fpath[1]}/_clove"
_clove() {
    local -a cmds
    cmds=(status list watch show add remove pause resume start verify priorities announce sequential peer completions)
    _arguments '1: :->command' '*:: :->args'
    case $state in
        command) _describe 'clove command' cmds ;;
        args)
            case $words[1] in
                add) _files ;;
                completions) _values 'shell' bash zsh fish ;;
                sequential) [[ $CURRENT -eq 3 ]] && _values 'setting' on off ;;
                # Torrents are named by info-hash or a unique prefix, which
                # nothing here can enumerate without talking to the daemon;
                # the flags are what completion can usefully offer.
                remove) _values 'flag' --all --data ;;
                pause|resume|start|verify|announce) _values 'flag' --all ;;
            esac
            ;;
    esac
}
_clove "$@"
