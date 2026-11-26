terraform {
  backend "s3" {
    bucket = "cress-general-data-975049957858-ap-northeast-1-an"
    key    = "terraform/private-archive/main/tfstate"
    region = "ap-northeast-1"
  }
}