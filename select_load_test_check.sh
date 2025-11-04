docker exec -it uptime-postgres psql -U uptime_user -d uptime_db -c "
SELECT unix_timestamp, count(*)
FROM uptime_logs
WHERE unix_timestamp >= EXTRACT(EPOCH FROM now()) - 20
GROUP BY unix_timestamp
ORDER BY 2 DESC, 1 DESC;
"

