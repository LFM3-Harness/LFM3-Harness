# Incident 214 postmortem

At 14:05 the checkout service returned errors. The root cause was an expired database certificate.
We detected the issue at 14:12 and restored service at 14:47.
Impact: 1,240 failed checkouts. No customer data was lost.

Actions: rotate certificates 30 days before expiry; add a certificate expiry alert; test the renewal runbook.
Owner: Priya. Due date: 15 November.
