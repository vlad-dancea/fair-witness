mkdir 'field notes'
printf 'sample,ph\nS01-1,6.4\n' > 'field notes/site 1 (copy).csv'
printf 'sample,ph\nS02-1,6.1\n' > 'Bodenproben Größe µm – Übersicht.csv'
printf 'sample,ph\nS03-1,6.3\n' > '土壤样本.csv'
printf 'sample,ph\nS04-1,6.0\n' > -n.csv
printf 'sample,ph\nS05-1,6.2\n' > $'two\nlines.csv'
printf 'sample,ph\nS06-1,6.5\n' > "$(printf 'L%.0s' $(seq 1 250)).csv"
: > empty.csv
mkdir empty-folder
wait_for_verdict

# Links to outside the project and to nothing, which the watcher lists but never follows.
ln -s /dev/null outside-link
ln -s / outside-folder-link
ln -s missing-target dangling-link
wait_for_verdict

# A name that is not valid UTF-8, and one that spells how fair-witness escapes it.
printf 'sample,ph\nS07-1,6.4\n' > $'latin1-\xe9t\xe9.csv'
printf 'sample,ph\nS08-1,6.1\n' > 'latin1-\xe9t\xe9.csv'
wait_for_verdict
