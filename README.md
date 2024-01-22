# pahkat-reposrv

## Intro

`pahkat-reposrv` is a server that provides an API for adding, removing, and updating pahkat packages in a given package index.

[An example package index managed by `pahkat-reposrv` looks like this](https://github.com/divvun/pahkat.uit.no-index/) (you might need access)

[`pahkat-uploader`](https://github.com/divvun/pahkat/tree/main/pahkat-uploader) can be used to upload new or modify existing packages. It uses the API provided by this repo to do so.

`pahkat-reposrv` uses 
 [`pahkat-repomgr`](https://github.com/divvun/pahkat/tree/main/pahkat-repmgr) to add, remove, and modify metadata in the git repo that stores all package metadata.

## Development

Setting up a local test environment requires some manual git setup, the use of `pahkat-repomgr`, and creating a config file for `pahkat-reposrv`.

Note that some of these steps seem like they should be handled by `pahkat-repomgr`, but that's not documented and this works.

Steps:

1. Create an empty directory that will hold repos/packages `mkdir pahkat-test-repo`
2. Initialize it as a git repo: `cd pahkat-test-repo` then `git init` (it's eventually going to complain about not being able to find origin, so if you want it to actually sync to an origin, you could create an empty git repo on github or otherwise and clone that instead)
3. Use `pahkat-repomgr` to create a new repo. This is confusing because it only sorta seems to do what you'd expect.
	1. `pahkat-repomgr repo init`
	2. at the prompt, for path use `/path/to/pahkat-test-repo/myrepo`. **Important:** make sure to include "myrepo" at the end of the path or it will not make a new directory. Also make sure to use absolute paths. It will not figure out what `~` means, for example, so use a path like `/Users/you/Desktop/pahkat-test-repo/myrepo`
	3. For Base URL, don't be fooled! It looks like it's filled in `https://` for you, doesn't it? It hasn't! You must type out the full url directly after the prompt that already contains `https://`. So the prompt will look deliciously like this when it's correct: `https://https://test.com`
	4. Enter a name for the repo
	5. Enter a description
4. Now that the repo is created, you have to manually commit the changes to `pahkat-test-repo` because it didn't do that for you, and if you don't, it will delete everything you just did the next time you run `pahkat-reposrv`. This is because it cleans up everything that isn't added to git each time it's run.
5. Now create a `Config.toml` by copying `Config.toml.example` and enter the following:
	1. `api_token = "your-api-token"` (this can be any string you choose)
	2. `git_path = "/path/to/pahkat-test-repo"`
	3. `repos = ["myrepo"]` assuming you named your repo `myrepo` in step 3.2
	4. set `url` and `host` to `localhost`
	5. `port = 9000`
6. Run it with `cargo run -- -c Config.toml`

### Creating a Package

Now that the server is running, you can create a new package by sending a POST request to the following URL:

`http://localhost:9000/<repo name>/packages/<new package name>`

Where `<repo name>` is what you named your repo in step 3.2,
`<new package name>` is a name you choose for your new package, and
`<my-api-token>` is what you set in step 5.1.

Example `curl` request:
```bash
# Create a package in `myrepo` called `my-first-package`
curl -X "POST" "http://localhost:9000/myrepo/packages/my-first-package" \
     -H 'Authorization: Bearer <my-api-token>' \
     -H 'Content-Type: application/json; charset=utf-8' \
     -d $'{
  "name": {
    "en": "English name",
    "es": "Spanish name"
  },
  "tags": [
    "test",
    "from-api"
  ],
  "description": {
    "en": "English description",
    "es": "Spanish description"
  }
}'

```

### Updating a Package

The below `curl` request sends a `PATCH` request that modifies the package created above to add a Swedish `name` and `description` and add a release. Note that for sake of illustration it contains an actual release that belongs to an actual package. Don't forget to add your API token.

```bash
# Update the package in `myrepo` called `my-first-package`
curl -X "PATCH" "http://localhost:9000/myrepo/packages/my-first-package" \
     -H 'Authorization: Bearer <my-api-token>' \
     -H 'Content-Type: application/json' \
     -d $'{
  "description": {
    "sv": "Swedish description",
    "en": "English description",
    "es": "Spanish description"
  },
  "channel": "nightly",
  "name": {
    "sv": "Swedish name",
    "en": "English name",
    "es": "Spanish name"
  },
  "target": {
    "platform": "macos",
    "dependencies": {},
    "payload": {
      "pkg_id": "no.uit.giella.keyboards.fit.keyboardlayout.fit",
      "requires_reboot": [
        "install",
        "uninstall"
      ],
      "size": 14140,
      "targets": [
        "system",
        "user"
      ],
      "type": "MacOSPackage",
      "installed_size": 1,
      "url": "https://pahkat.uit.no/artifacts/keyboard-fit_0.1.0-nightly.20240108T001817216Z_macos.pkg"
    }
  },
  "version": "0.1.0-nightly.20240108T001817216Z"
}'
```

---
The below wasn't necessary when following the above steps. Leaving in case it's helpful:

>When developing and needing to modify the repo contents, or otherwise do strange things, you can self-host locally with Caddy.
>
>```bash
>caddy reverse-proxy --to localhost:9000
>```

---

## Deploying a Release

The following steps will create a new release and immediately deploy it to the production server.

Let's say you wanted to release version 1.6.9. You'd do this:

1. Update the version number in `Cargo.toml` to 1.6.9 and commit your changes
2. `git commit -m "Bump version to 1.6.9"`
3. Create a tag with the same version you set in `Cargo.toml`, for example: `git tag 1.6.9`
4. Push the tag *before* pushing main:`git push 1.6.9`
5. `git push`

:warning: **Beware:** your git client might betray you. It's recommended when releasing a new version to follow the steps exactly as above from the command line.