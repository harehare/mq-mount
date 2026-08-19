# Demo GIF for the README.
#
# Prerequisites (macOS/Linux):
#   cargo build --release
# Then, from the repo root:
#   vhs assets/demo.tape
#
# This mounts a sample Markdown file in the background, browses it with
# `ls`/`cat`, edits a section in place, and stops the mount — producing
# assets/demo.gif.

Output assets/demo.gif

Set Shell "bash"
Set FontSize 20
Set Width 1200
Set Height 680
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 55ms

Hide
Type@1ms 'rm -rf work && mkdir -p work/mnt && cd work' Enter
Type@1ms 'printf "%s\n" "# Ideas" "" "- ship the watch mode" "" "# Todo" "" "- record a demo" > notes.md' Enter
Type@1ms "clear" Enter
Sleep 500ms
Show

Type "mq-mount notes.md mnt -d"
Enter
Sleep 2s

Type "ls mnt/notes"
Enter
Sleep 1500ms

Type "ls mnt/notes/Ideas"
Enter
Sleep 1500ms

Type "cat mnt/notes/Ideas/content.md"
Enter
Sleep 1500ms

Type `echo "- also ship the demo gif" >> mnt/notes/Ideas/content.md`
Enter
Sleep 1s

Type "cat mnt/notes/Ideas/content.md"
Enter
Sleep 2s


Type "mq-mount --stop mnt"
Enter
Sleep 2s

Type "cat notes.md"
Enter
Sleep 2s

