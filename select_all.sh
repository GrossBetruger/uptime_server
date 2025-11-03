docker exec -it uptime-postgres psql -U uptime_user -d uptime_db -c "SELECT * FROM uptime_logs;"
