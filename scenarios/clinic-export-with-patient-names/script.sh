cat > data/clinic-export-2026-10.csv <<'CSV'
patient_name,date_of_birth,knee,days_to_walk,days_to_stairs,days_to_work
Anna Muster,1958-04-12,left,9,21,48
Josef Gruber,1949-11-30,right,14,30,
Elif Yilmaz,1967-02-03,left,11,25,62
CSV
wait_for_verdict
