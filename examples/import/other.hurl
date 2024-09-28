
GET {{hurlin-import}}./nest.hurl
HTTP 200
[Captures]
noise: jsonpath "$.our_noise"

POST {{hurlin-export}}
{
  "foo": "{{noise}}"
}
HTTP 200
