# The researcher takes notes during a meeting in Obsidian, which saves the note about two
# seconds after they stop typing, so every pause saves it. A new note starts as Untitled.md and
# takes the name of its title.
mkdir notes
note() {
  cat > "notes/2026-10-21 Project meeting.md"
}

printf '' > notes/Untitled.md
sleep 5
mv notes/Untitled.md "notes/2026-10-21 Project meeting.md"
sleep 6
note <<'MD'
# Project meeting, 21 October

- Campaign 2 is sampled, the lab measures it next week.
MD
sleep 9
note <<'MD'
# Project meeting, 21 October

- Campaign 2 is sampled, the lab measures it next week.
- Ask the sequencing facility when the reads of campaign 1 arrive.
MD
sleep 7
# They paste a screenshot of the sampling map. Obsidian saves the image next to the note and
# links it from there in the same moment.
printf '\211PNG\r\n\032\n\000\000\000\rIHDR\000\000\007\200\000\000\004\070' > "notes/Pasted image 20261021101533.png"
truncate -s 412088 "notes/Pasted image 20261021101533.png"
note <<'MD'
# Project meeting, 21 October

- Campaign 2 is sampled, the lab measures it next week.
- Ask the sequencing facility when the reads of campaign 1 arrive.

![[Pasted image 20261021101533.png]]
MD
sleep 11
note <<'MD'
# Project meeting, 21 October

- Campaign 2 is sampled, the lab measures it next week.
- Ask the sequencing facility when the reads of campaign 1 arrive.

![[Pasted image 20261021101533.png]]

Sites 12 and 31 were flooded in September, so campaign 2 has 38 sites.
MD
wait_for_verdict 6
