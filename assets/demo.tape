Output "assets/demo.gif"

Set Shell "bash"
Set FontFamily "Maple Mono NF CN"
Set FontSize 16
Set Width 1000
Set Height 500
Set Padding 20
Set Theme "Catppuccin Mocha"
Set Framerate 20
Set TypingSpeed 40ms
Set CursorBlink false

Set TypingSpeed 0ms
Hide
Type 'export PATH="$PWD/target/release:$PATH"'
Enter
Type 'export GQ_NVIDIA_SMI="$PWD/assets/demo/nvidia-smi"'
Enter
Type "export PS1='$ '"
Enter
Type 'clear'
Enter
Show
Sleep 400ms
Set TypingSpeed 40ms

Type "gq topo"
Enter
Sleep 2.5s

Type "gq list"
Enter
Sleep 2.5s

Type "gq run -k 2 --strategy topology-aware --dry-run"
Enter
Sleep 2s

Type "gq run -k 2 --strategy min-used --dry-run"
Enter
Sleep 2s

Type "gq run -k 2 -- sh -c 'echo $CUDA_VISIBLE_DEVICES'"
Enter
Sleep 2.5s
