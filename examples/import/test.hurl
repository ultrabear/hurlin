GET {{hurlin-import}}../import/other.hurl
HTTP 200
[Captures]
json_key: jsonpath "$.foo"


GET {{hurlin-import}}./other.hurl
HTTP 200 
[Captures]
json_key: jsonpath "$.foo"
