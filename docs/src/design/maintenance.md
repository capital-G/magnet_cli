# Maintenance

While most effort is to create all these tools in the first place, there is also some continuous maintenance effort necessary.

* Since the Django server is exposed to the internet, it is necessary to keep this up-to-date. The server and its assets should also be backed-up regularly!
* The client only needs update in case we want to extend the existing manifest format or change how the API is exposed. Since we link OpenSSL dynamically, it should also obtain the most crucial security and certificate updates.

Additional to this, there is also some cost associated with providing the server to host and serve data:

* The server load itself is probably not too high, so it is probably fine with running on a machine with 8GB and 8 cores. Currently, such a server costs around 10 Euro/month. For the beginning I will host it on my own server which has plenty of resources available.
* The object storage scales differently: I will install Garage on my server and will give it up to 400 GB of storage. The moment this becomes too small we need to figure out if we switch to something S3 compatible - e.g. Hetzner provides a S3 plan which costs for 1TB of storage and 3TB of traffic around 10 Euro/month.

It would also be great if there would be some people doing moderation work to resolve reports of packages etc.
