# clove(1) fish completion.
# Install: clove completions fish > ~/.config/fish/completions/clove.fish
complete -c clove -f
set -l clove_cmds status list watch show add remove pause resume verify priorities announce sequential peer completions
complete -c clove -n '__fish_use_subcommand' -a "$clove_cmds"
complete -c clove -n '__fish_seen_subcommand_from add' -F
complete -c clove -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish'
complete -c clove -n '__fish_seen_subcommand_from sequential' -a 'on off'
