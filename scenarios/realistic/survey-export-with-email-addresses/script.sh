# Stands in for the browser's Downloads folder, outside the project.
downloads=$(mktemp -d)
trap 'rm -r "$downloads"' EXIT

# Writes a LimeSurvey CSV export of wave 2. With "details", the export also holds the names and
# email addresses from the participant table.
export_wave_2() {
  awk -v details="$1" 'BEGIN {
    split("Sabine Karim Ursula Dragan Johanna Mehmet Elena Thomas Ayse Lukas Maria Stefan Fatima Georg Ivana Peter", first, " ")
    split("Novak Haddad Pichler Petrovic Mayr Kaya Gruber Huber Yilmaz Wagner Bauer Steiner Hassan Moser Horvat Fuchs", last, " ")
    split("10 15 16 20", district, " ")
    printf "\"id\",\"submitdate\",\"lastpage\",\"startlanguage\",\"seed\",\"token\","
    if (details) printf "\"firstname\",\"lastname\",\"email\","
    print "\"district\",\"building\",\"floor\",\"Q01\",\"Q02\",\"Q03\",\"Q04\",\"Q05\",\"Q06\",\"Q07\",\"Q08\""
    for (i = 1; i <= 240; i++) {
      f = first[i % 16 + 1]; l = last[(i * 7) % 16 + 1]
      printf "\"%d\",\"2026-10-%02d %02d:%02d:%02d\",\"3\",\"de\",\"%d\",\"R-%04d\",", i, 12 + i % 9, 8 + i % 12, i % 60, (i * 13) % 60, 1000000 + i * 7919, i
      if (details) printf "\"%s\",\"%s\",\"%s.%s@example.net\",", f, l, tolower(f), tolower(l)
      printf "\"%s\",\"%c\",\"%d\",\"%d\",\"%s\",\"%s\",\"%s\",\"A%d\",\"A%d\",\"%d\",\"%s\"\n", district[i % 4 + 1], 65 + i % 6, 1 + i % 7, i % 9, (i % 3 ? "N" : "Y"), (i % 4 ? "Y" : "N"), (i % 5 ? "Y" : "N"), 1 + i % 5, 1 + (i * 3) % 5, i % 11, (i % 6 ? "Y" : "N")
    }
  }'
}

# The researcher exports wave 2 with the participant details ticked by mistake, and moves the
# download from Downloads into the project under a better name.
export_wave_2 1 > "$downloads/results-survey284913.csv"
mv "$downloads/results-survey284913.csv" survey/responses-wave-2.csv
sleep 6
# They open it in LibreOffice Calc, which locks it, look at it for half a minute and close it.
printf ',selin,lab-pc-07,26.10.2026 10:12,file:///home/selin/.config/libreoffice/4;' \
  > 'survey/.~lock.responses-wave-2.csv#'
sleep 30
rm 'survey/.~lock.responses-wave-2.csv#'
wait_for_verdict

# They notice the names and email addresses, export the wave again without them and copy it
# over the file in the project.
export_wave_2 0 > "$downloads/results-survey284913 (1).csv"
cp "$downloads/results-survey284913 (1).csv" survey/responses-wave-2.csv
wait_for_verdict
