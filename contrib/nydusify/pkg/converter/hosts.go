// Copyright 2022 Nydus Developers. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

package converter

import (
	"strings"

	"github.com/goharbor/acceleration-service/pkg/remote"
)

func hosts(opt Opt) remote.HostFunc {
	maps := map[string]bool{
		authority(opt.Source):       opt.SourceInsecure,
		authority(opt.Target):       opt.TargetInsecure,
		authority(opt.ChunkDictRef): opt.ChunkDictInsecure,
		authority(opt.CacheRef):     opt.CacheInsecure,
	}
	return func(ref string) (remote.CredentialFunc, bool, error) {
		return remote.NewDockerConfigCredFunc(), maps[authority(ref)], nil
	}
}

func authority(ref string) string {
	i := strings.Index(ref, "/")
	if i > 0 {
		return ref[:i]
	} else {
		return ref
	}
}
