# The recording of docs/demo/demo.gif. Run it with record.sh, which prepares the project.
Output docs/demo/demo.gif

Set Shell bash
Set FontSize 16
Set Width 1100
Set Height 500
Set Padding 16
Set Framerate 20
Set TypingSpeed 60ms
Set WaitTimeout 5m

Hide
Type "exec docs/demo/record.sh sandbox"
Enter
Wait+Screen /urban-heat \$/
Type "fair-witness setup --url https://damap.it.tuwien.ac.at --ai-url https://aqueduct.ai.datalab.tuwien.ac.at/v1 --model glm-5.3-744b-experimental"
Enter
Wait+Screen /Username/
Type "user"
Enter
Wait+Screen /Password/
Type "user"
Enter
Wait+Screen /DMP for this folder/
Type "#2751 "
Enter
Wait+Screen /to start watching this folder/
# run uses the key setup saved.
Type "unset FAIR_WITNESS_API_KEY; researcher >/dev/null 2>&1 & clear"
Enter
Sleep 1s
Show

Sleep 1s
Type "fair-witness run"
Sleep 500ms
Enter
Wait+Screen /checking against the DMP/
Sleep 3s
# Skip the wait for the model, which the spinner's seconds show.
Hide
Wait+Screen /↳/
Show
Wait+Screen /checking against the DMP/
Sleep 3s
Hide
Wait+Screen /(?s)↳.*↳/
Show
Sleep 8s
