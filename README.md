# Hurlin; extending Hurl
Hurlin is a program that allows completely valid hurl files to use imports and basic concurrency by running a local webserver and injecting variables into each hurl invocation;
```hurl
GET {{hurlin-import}}./new-session.hurl
HTTP 200
[Captures]
token: jsonpath "$.token"
username: jsonpath "$.username"
```
`new-session.hurl`:
```hurl
GET https://login.myapi.com/session
HTTP 200
[Captures]
token: cookie "token"
username: jsonpath "$.username"

POST {{hurlin-export}}
{
    "token": "{{token}}",
    "username": "{{username}}"
}
HTTP 200
```

## Concurrency
```hurl
GET {{hurlin-async}}./biglatency.hurl
HTTP 200
[Captures]
biglatency: jsonpath "$.await"

GET {{hurlin-async}}./test-from-australia.hurl
HTTP 200
[Captures]
infinite_ping: jsonpath "$.await"


GET {{hurlin-await}}{{biglatency}}
HTTP 200
[Captures]
mydata: jsonpath "$.answer"

GET {{hurlin-await}}{{infinite_ping}}
HTTP 200
[Captures]
kangaroo: jsonpath "$.kangaroo"
```


