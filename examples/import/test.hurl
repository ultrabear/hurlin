GET {{hurlin-async}}./other.hurl
HTTP 200
[Captures]
key: jsonpath "$.await"


GET {{hurlin-await}}{{key}}
HTTP 200
[Captures]
json_key: jsonpath "$.foo"
