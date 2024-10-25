## http-app

#### Why does this exist?
For many of my http servers I was directly using hyper to keep things simple. Then hyper changed its API and made upgrading a pain. This is simple wrapper around hyper to make it easy to create a simple HTTP server which will hopefully have a more stable API while hyper and other libs continue to experiment.
