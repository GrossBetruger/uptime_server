docker exec -it uptime-postgres psql -U uptime_user -d uptime_db -c "SELECT unix_timestamp, count(*) FROM uptime_logs group by unix_timestamp order by 2 desc, 1 desc;"
