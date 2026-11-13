# Records docs/images/demo.gif from fake threads. Needs VHS (https://github.com/charmbracelet/vhs).
# Run from the repo root:
#   cargo build --release && vhs demo/demo.tape

Output docs/images/demo.gif

Set Shell bash
Set FontSize 18
Set Width 1300
Set Height 440
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 60ms

Hide
Type "export PS1='$ ' PATH=$PWD/target/release:$PATH THREADS_ROOT=/tmp/threads-demo/projects THREADS_INDEX=/tmp/threads-demo/index.db" Enter
Type "rm -rf /tmp/threads-demo && python3 demo/make_demo.py /tmp/threads-demo && clear" Enter
Sleep 500ms
Show

Type "threads update" Enter
Sleep 1.5s

Type "threads search migration roll" Enter
Sleep 4s
Type "clear" Enter

Type "threads search ABC-317" Enter
Sleep 4s
Type "clear" Enter

Type "threads stats" Enter
Sleep 3s
