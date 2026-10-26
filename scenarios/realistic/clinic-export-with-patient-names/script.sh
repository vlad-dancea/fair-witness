# The clinic sends its export of October as a ZIP attachment. Builds it outside the project.
outside=$(mktemp -d)
awk 'BEGIN {
  split("Anna Josef Elif Maria Franz Hermine Mustafa Gerda Walter Ingrid Dusan Renate", first, " ")
  split("Muster Gruber Yilmaz Huber Wagner Bauer Demir Pichler Steiner Moser Jovanovic Fuchs", last, " ")
  print "patient_id;patient_name;date_of_birth;sex;surgery_date;knee;days_to_walk;days_to_stairs;days_to_work"
  for (i = 1; i <= 120; i++) {
    printf "KH-%05d;%s %s;19%02d-%02d-%02d;%s;2026-%02d-%02d;%s;%d;%d;%s\n", 40000 + i * 17,
      first[i % 12 + 1], last[(i * 5) % 12 + 1], 45 + i % 25, 1 + i % 12, 1 + i % 28,
      (i % 2 ? "F" : "M"), 7 + i % 3, 1 + i % 28, (i % 3 ? "left" : "right"), 7 + i % 9,
      18 + i % 15, (i % 4 ? 40 + i % 30 : "")
  }
}' > "$outside/clinic-export-2026-10.csv"
(cd "$outside" && zip -q clinic-export-2026-10.zip clinic-export-2026-10.csv)

# The researcher saves the attachment from the mail client into the data folder, extracts it a
# few seconds later and deletes the archive.
cp "$outside/clinic-export-2026-10.zip" data/
sleep 6
unzip -q data/clinic-export-2026-10.zip -d data
sleep 8
rm data/clinic-export-2026-10.zip
wait_for_verdict 3
