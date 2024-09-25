GET {{hurlin-import}}./other.hurl
HTTP 200
[Captures]
json_key: jsonpath "$.foo"


POST {{hurlin-export}}
{
  "more data": "other stuff"
}
HTTP 200
