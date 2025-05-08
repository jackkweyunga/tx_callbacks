## Receive API callbacks via Websocket Connection

You can test this API with:

A tool like curl to send callbacks:

```bash
curl -X POST -H "Content-Type: application/json" -d '{"status":"success","data":{"amount":100}}' http://localhost:3000/api/callback/tx-123
```

A WebSocket client connected to:

ws://localhost:5000/ws/callback/tx-123

