#!/usr/bin/env sh

docker build -t breakcoremoose . && docker tag breakcoremoose seidenschnabel2k/breakcoremoose && docker push seidenschnabel2k/breakcoremoose
