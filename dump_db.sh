
docker exec -i uptime-postgres psql -U uptime_user -d uptime_db -c "COPY (SELECT * FROM uptime_logs) TO STDOUT WITH (FORMAT csv, HEADER true)" > uptime_logs.csv
