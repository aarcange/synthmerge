spec_template   ?= .distro/synthmerge.spec.template
spec		:= dist/synthmerge.spec

PKG_VERSION	:= $(shell grep '^version = ' Cargo.toml | sed 's,.*\"\(.*\)\",\1,')
PKG_TARBALL	:= dist/synthmerge-$(PKG_VERSION).tar.gz

default:
	@echo "targets: dist srpm clean"

.PHONY: dist
dist tarball $(PKG_TARBALL):
	rm -rf dist
	mkdir dist
	tar -czf $(PKG_TARBALL) --exclude ".git" --exclude "dist" --exclude ".distro" --exclude "release/*" --transform "s,^./,synthmerge-$(PKG_VERSION)/," .

srpm: $(PKG_TARBALL)
	dnf -y install git
	rm -rf rpm
	mkdir rpm
	PKG_RELEASE=$$(( $$(git rev-list $$(git describe --tags --abbrev=0)..HEAD --count) + 1 )); \
	sed -e "s,@@SYNTHMERGE-VERSION@@,$(PKG_VERSION)," -e "s,@@SYNTHMERGE-RELEASE@@,$$PKG_RELEASE," $(spec_template) > $(spec)
	rpmbuild -bs --define "_sourcedir dist" --define "_srcrpmdir $(outdir)" $(spec)

clean:
	rm -rf $(PKG_TARBALL) dist rpm
