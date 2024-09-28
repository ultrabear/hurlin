
GET {{hurlin-noise}}
HTTP 200
[Captures]
noise: jsonpath "$.noise"

POST {{hurlin-export}}
{
  "our_noise": "{{noise}}"
}
HTTP 200
