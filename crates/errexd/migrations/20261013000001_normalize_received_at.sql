UPDATE events
SET received_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', received_at)
WHERE received_at NOT LIKE '%T%';
