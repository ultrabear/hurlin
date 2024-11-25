# Hurlin; extending Hurl
Hurlin is a program that allows completely valid hurl files (run using a real hurl binary!) to use imports and basic concurrency by running a local webserver and injecting variables into each hurl invocation;
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

# The Hurlin Model
Hurlin is conceptually a runtime for hurl files, referred to as "hurlin tasks". Tasks are either spawned synchronously (`hurlin-import`), asynchronously (`hurlin-async`) or are the "root hurlin task" (passed as argument to hurlin cli).

When a hurl file is imported, synchronously or asynchronously, it becomes one of two things: an immediately ready cached result, or a new hurlin task. 
Once a hurlin task runs to completion, its `hurlin-export` (or lack of data if `hurlin-export` is not called) is cached by the runtime, and subsequent calls to `hurlin-import` or `hurlin-async` will resolve immediately to that cached value.

This style of importing data allows for constructing dependency trees in tests/data seeders. For instance; 5 users are created in `create_users.hurl` and both `create_blogs.hurl` and `create_comments.hurl` want to access the credentials of the 5 created users or rely on those users being created, in hurlin, both files can `GET {{hurlin-import}}./create_users.hurl` and ensure that those users were created, without causing a server issue from duplicate users being created (or worse, 10 users silently being created, each with different credentials that do not line up).

Caching can be opted out of on a per import/asyncimport basis using the `?nocache` query parameter, in this mode, the imported file will neither be cached nor will the cache be read from to skip importing.

If a hurlin task imports another hurl file that is currently executing in a cache enabled task, it will seamlessly block until that task completes, returning any `{{hurlin-export}}`'ed data as if it were the first caller, this implies an alternate form to `{{hurlin-async}}./file.hurl`/`{{hurlin-await}}{{fileref}}` syntax: `{{hurlin-async}}./file.hurl`/`{{hurlin-import}}./file.hurl`. Note that await keys given by a call to `{{hurlin-async}}` are only valid within the original task that called `{{hurlin-async}}`, and attempting to await on the same promise twice will result in an error.

Because hurlin *is* just running hurl files (currently using a real hurl binary!) it is possible to ignore any errors from a hurlin call by simply not asserting `HTTP 200`, it is sane practice to assert `HTTP 200` on any call to hurlin, to ensure errors propagate and do not cause potentially wild inconsistencies, but it should be noted there is nothing preventing skipping assertions, or even asserting on Hurlin giving a specific error code. Hurlin's error codes are *not stable*, but they will not fall in the 2xx range of HTTP responses.


# Current API
hurlin currently supports these API's:
- `GET {{hurlin-import}}./filepath`: Import another hurl file, blocking until completion
  - Params: `?nocache`: If a hurl file has been previously imported, it's export call is cached and returned immediately with a `200`, nocache prevents this behaviour on both sides, by running unconditionally and not setting the cache on completion
  - Returns `200` if hurl file successfully ran to completion, and mirrors body and Content-Type if the imported file called `{{hurlin-export}}`
- `GET {{hurlin-async}}./filepath`: Import another hurl file, but do not block until completion
  - Params: `?nocache`: If a hurl file has been previously imported, it's export call is cached and returned immediately with a `200`, nocache prevents this behaviour on both sides, by running unconditionally and not setting the cache on completion
  - Returns `200` if hurl file existed and has begun running, returns `application/json` in the form: `{ await: string }` where await may be used to wait on the tasks completion and get exports
- `GET {{hurlin-readfile}}./filepath`: Read a file and return its contents in the HTTP body
  - Returns `200` if file existed and file contents in the HTTP body, does not set Content-Type
- `GET {{hurlin-await}}{{await}}`: Wait on an asynchronously spawned hurl files completion, the `{{await}}` key is derived from a call to `{{hurlin-async}}`'s response body
  - Returns `200` if the hurl file successfully ran to completion, and mirrors body and Content-Type if the imported file called `{{hurlin-export}}`
- `POST {{hurlin-export}}`: exports data for hurlin tasks importing this task to read
  - Returns `200` unconditionally, stores POST'ed body and Content-Type for any importers of this task
- `GET {{hurlin-noise}}`: Returns a short amount of random data for seeding test data
  - Returns `200` unconditionally, sends `application/json` in the form `{ noise: string }` where noise is a 16 byte hex value
